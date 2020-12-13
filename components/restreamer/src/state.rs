use std::{future::Future, panic::AssertUnwindSafe, path::Path};

use anyhow::anyhow;
use derive_more::{Display, From};
use ephyr_log::log;
use futures::{
    future::TryFutureExt as _,
    sink,
    stream::{StreamExt as _, TryStreamExt as _},
};
use futures_signals::signal::{Mutable, SignalExt as _};
use juniper::{GraphQLEnum, GraphQLObject, GraphQLScalarValue, GraphQLUnion};
use serde::{Deserialize, Serialize};
use smart_default::SmartDefault;
use tokio::{fs, io::AsyncReadExt as _};
use url::Url;
use uuid::Uuid;
use xxhash::xxh3::xxh3_64;

use crate::{display_panic, srs};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct State {
    pub password_hash: Mutable<Option<String>>,
    pub restreams: Mutable<Vec<Restream>>,
}

impl State {
    pub async fn try_new<P: AsRef<Path>>(
        file: P,
    ) -> Result<Self, anyhow::Error> {
        let file = file.as_ref();

        let mut contents = vec![];
        let _ = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .read(true)
            .open(&file)
            .await
            .map_err(|e| {
                anyhow!("Failed to open '{}' file: {}", file.display(), e)
            })?
            .read_to_end(&mut contents)
            .await
            .map_err(|e| {
                anyhow!("Failed to read '{}' file: {}", file.display(), e)
            })?;

        let state = if contents.is_empty() {
            State::default()
        } else {
            serde_json::from_slice(&contents).map_err(|e| {
                anyhow!(
                    "Failed to deserialize state from '{}' file: {}",
                    file.display(),
                    e,
                )
            })?
        };

        let (file, persisted_state) = (file.to_owned(), state.clone());
        let persist_state1 = move || {
            fs::write(
                file.clone(),
                serde_json::to_vec(&persisted_state)
                    .expect("Failed to serialize server state"),
            )
            .map_err(|e| log::error!("Failed to persist server state: {}", e))
        };
        let persist_state2 = persist_state1.clone();
        Self::on_change("persist_restreams", &state.restreams, move |_| {
            persist_state1()
        });
        Self::on_change(
            "persist_password_hash",
            &state.password_hash,
            move |_| persist_state2(),
        );

        Ok(state)
    }

    pub fn on_change<F, Fut, T>(name: &'static str, val: &Mutable<T>, hook: F)
    where
        F: FnMut(T) -> Fut + Send + 'static,
        Fut: Future + Send + 'static,
        T: Clone + PartialEq + Send + Sync + 'static,
    {
        let _ = tokio::spawn(
            AssertUnwindSafe(
                val.signal_cloned().dedupe_cloned().to_stream().then(hook),
            )
            .catch_unwind()
            .map_err(move |p| {
                log::crit!(
                    "Panicked executing `{}` hook of state: {}",
                    name,
                    display_panic(&p),
                )
            })
            .map(|_| Ok(()))
            .forward(sink::drain()),
        );
    }

    #[must_use]
    pub fn add_pull_input(
        &self,
        src: Url,
        replace_id: Option<InputId>,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();

        for r in &*restreams {
            if let Input::Pull(i) = &r.input {
                if &src == &i.src && replace_id != Some(r.id) {
                    return Some(false);
                }
            }
        }

        Self::add_input_to(
            &mut *restreams,
            Input::Pull(PullInput {
                src,
                status: Status::Offline,
            }),
            replace_id,
        )
    }

    #[must_use]
    pub fn add_push_input(
        &self,
        name: String,
        replace_id: Option<InputId>,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();

        for r in &*restreams {
            if let Input::Push(i) = &r.input {
                if &name == &i.name && replace_id != Some(r.id) {
                    return Some(false);
                }
            }
        }

        Self::add_input_to(
            &mut *restreams,
            Input::Push(PushInput {
                name,
                status: Status::Offline,
            }),
            replace_id,
        )
    }

    fn add_input_to(
        restreams: &mut Vec<Restream>,
        input: Input,
        replace_id: Option<InputId>,
    ) -> Option<bool> {
        if let Some(id) = replace_id {
            let r = restreams.iter_mut().find(|r| r.id == id)?;
            if !r.input.is(&input) {
                r.input = input;
                r.srs_publisher_id = None;
                for o in &mut r.outputs {
                    o.status = Status::Offline;
                }
            }
        } else {
            restreams.push(Restream {
                id: InputId::new(),
                input,
                outputs: vec![],
                enabled: true,
                srs_publisher_id: None,
            });
        }
        Some(true)
    }

    #[must_use]
    pub fn remove_input(&self, id: InputId) -> bool {
        let mut restreams = self.restreams.lock_mut();
        let prev_len = restreams.len();
        restreams.retain(|r| r.id != id);
        restreams.len() != prev_len
    }

    #[must_use]
    pub fn enable_input(&self, id: InputId) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let input = restreams.iter_mut().find(|r| r.id == id)?;

        if input.enabled {
            return Some(false);
        }

        input.enabled = true;
        Some(true)
    }

    #[must_use]
    pub fn disable_input(&self, id: InputId) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let input = restreams.iter_mut().find(|r| r.id == id)?;

        if !input.enabled {
            return Some(false);
        }

        input.enabled = false;
        input.srs_publisher_id = None;
        Some(true)
    }

    #[must_use]
    pub fn add_new_output(
        &self,
        input_id: InputId,
        output_dst: Url,
        label: Option<String>,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let outputs =
            &mut restreams.iter_mut().find(|r| r.id == input_id)?.outputs;

        if outputs.iter_mut().find(|o| &o.dst == &output_dst).is_some() {
            return Some(false);
        }

        outputs.push(Output {
            id: OutputId::new(),
            dst: output_dst,
            label,
            enabled: false,
            status: Status::Offline,
        });
        Some(true)
    }

    #[must_use]
    pub fn remove_output(
        &self,
        input_id: InputId,
        output_id: OutputId,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let outputs =
            &mut restreams.iter_mut().find(|r| r.id == input_id)?.outputs;

        let prev_len = outputs.len();
        outputs.retain(|o| o.id != output_id);
        Some(outputs.len() != prev_len)
    }

    #[must_use]
    pub fn enable_output(
        &self,
        input_id: InputId,
        output_id: OutputId,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let output = &mut restreams
            .iter_mut()
            .find(|r| r.id == input_id)?
            .outputs
            .iter_mut()
            .find(|o| o.id == output_id)?;

        if output.enabled {
            return Some(false);
        }

        output.enabled = true;
        Some(true)
    }

    #[must_use]
    pub fn disable_output(
        &self,
        input_id: InputId,
        output_id: OutputId,
    ) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        let output = &mut restreams
            .iter_mut()
            .find(|r| r.id == input_id)?
            .outputs
            .iter_mut()
            .find(|o| o.id == output_id)?;

        if !output.enabled {
            return Some(false);
        }

        output.enabled = false;
        Some(true)
    }

    #[must_use]
    pub fn enable_all_outputs(&self, input_id: InputId) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        Some(
            restreams
                .iter_mut()
                .find(|r| r.id == input_id)?
                .outputs
                .iter_mut()
                .filter(|o| !o.enabled)
                .fold(false, |_, o| {
                    o.enabled = true;
                    true
                }),
        )
    }

    #[must_use]
    pub fn disable_all_outputs(&self, input_id: InputId) -> Option<bool> {
        let mut restreams = self.restreams.lock_mut();
        Some(
            restreams
                .iter_mut()
                .find(|r| r.id == input_id)?
                .outputs
                .iter_mut()
                .filter(|o| o.enabled)
                .fold(false, |_, o| {
                    o.enabled = false;
                    true
                }),
        )
    }
}

#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct Restream {
    pub id: InputId,
    pub input: Input,
    pub outputs: Vec<Output>,
    pub enabled: bool,
    #[graphql(skip)]
    #[serde(skip)]
    pub srs_publisher_id: Option<srs::ClientId>,
}

#[derive(
    Clone, Debug, Deserialize, Eq, From, GraphQLUnion, PartialEq, Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    Push(PushInput),
    Pull(PullInput),
}

impl Input {
    #[inline]
    #[must_use]
    pub fn is_pull(&self) -> bool {
        matches!(self, Input::Pull(_))
    }

    #[inline]
    #[must_use]
    pub fn status(&self) -> Status {
        match self {
            Self::Pull(i) => i.status,
            Self::Push(i) => i.status,
        }
    }

    #[inline]
    pub fn set_status(&mut self, new: Status) {
        match self {
            Self::Pull(i) => i.status = new,
            Self::Push(i) => i.status = new,
        }
    }

    #[inline]
    #[must_use]
    pub fn is(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Pull(a), Self::Pull(b)) => a.is(b),
            (Self::Push(a), Self::Push(b)) => a.is(b),
            _ => false,
        }
    }

    #[inline]
    #[must_use]
    pub fn hash(&self) -> u64 {
        match self {
            Self::Pull(i) => xxh3_64(i.src.as_ref().as_bytes()),
            Self::Push(i) => xxh3_64(i.name.as_bytes()),
        }
    }

    #[inline]
    #[must_use]
    pub fn upstream_url(&self) -> Option<&Url> {
        if let Self::Pull(i) = self {
            Some(&i.src)
        } else {
            None
        }
    }

    #[inline]
    #[must_use]
    pub fn upstream_url_hash(&self) -> Option<u64> {
        self.upstream_url().map(|u| xxh3_64(u.as_ref().as_bytes()))
    }

    #[must_use]
    pub fn srs_url(&self) -> Url {
        Url::parse(&match self {
            Self::Pull(_) => {
                format!("rtmp://127.0.0.1:1935/pull_{}/in", self.hash())
            }
            Self::Push(i) => format!("rtmp://127.0.0.1:1935/{}/in", i.name),
        })
        .unwrap()
    }

    #[inline]
    #[must_use]
    pub fn srs_url_hash(&self) -> u64 {
        xxh3_64(self.srs_url().as_ref().as_bytes())
    }

    #[inline]
    #[must_use]
    pub fn uses_srs_app(&self, app: &str) -> bool {
        match self {
            Self::Pull(_) => {
                app.starts_with("pull_") && app[5..].parse() == Ok(self.hash())
            }
            Self::Push(i) => app == &i.name,
        }
    }
}

#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct PullInput {
    pub src: Url,
    #[serde(skip)]
    pub status: Status,
}

impl PullInput {
    #[inline]
    #[must_use]
    pub fn is(&self, other: &Self) -> bool {
        &self.src == &other.src
    }
}

#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct PushInput {
    pub name: String,
    #[serde(skip)]
    pub status: Status,
}

impl PushInput {
    #[inline]
    #[must_use]
    pub fn is(&self, other: &Self) -> bool {
        &self.name == &other.name
    }
}

#[derive(
    Clone, Debug, Deserialize, Eq, GraphQLObject, PartialEq, Serialize,
)]
pub struct Output {
    pub id: OutputId,
    pub dst: Url,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub enabled: bool,
    #[serde(skip)]
    pub status: Status,
}

impl Output {
    #[inline]
    #[must_use]
    pub fn is(&self, other: &Self) -> bool {
        &self.dst == &other.dst
    }

    #[inline]
    #[must_use]
    pub fn hash(&self) -> u64 {
        xxh3_64(self.dst.as_ref().as_bytes())
    }
}

#[derive(Clone, Copy, Debug, Eq, GraphQLEnum, PartialEq, SmartDefault)]
pub enum Status {
    #[default]
    Offline,
    Initializing,
    Online,
}

/// ID of an [`Input`].
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    Eq,
    GraphQLScalarValue,
    PartialEq,
    Serialize,
)]
pub struct InputId(Uuid);

impl InputId {
    /// Generates new random [`InputId`].
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

/// ID of an [`Output`].
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Display,
    Eq,
    GraphQLScalarValue,
    PartialEq,
    Serialize,
)]
pub struct OutputId(Uuid);

impl OutputId {
    /// Generates new random [`OutputId`].
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}
