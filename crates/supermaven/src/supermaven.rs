mod messages;
mod supermaven_completion_provider;

use anyhow::{Context as _, Result};
use collections::BTreeMap;
use futures::{channel::mpsc, io::BufReader, AsyncBufReadExt, StreamExt};
use gpui::{
    AppContext, AsyncAppContext, Bounds, DevicePixels, EntityId, Global, InteractiveText, Model,
    Render, StyledText, Task, ViewContext,
};
use language::{language_settings::all_language_settings, Anchor, Buffer, ToOffset};
use messages::*;
use postage::watch;
use serde::{Deserialize, Serialize};
use settings::SettingsStore;
use smol::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin, ChildStdout, Command},
};
use std::{ops::Range, path::PathBuf, process::Stdio};
pub use supermaven_completion_provider::*;
use ui::prelude::*;
use util::ResultExt;

pub fn init(cx: &mut AppContext) {
    cx.set_global(Supermaven::Disabled);

    todo!("initialize");

    // Old API before we migrated to a new InlineCompletionProvider setup
    //
    // let mut provider = all_language_settings(None, cx).inline_completions.provider;
    // if provider == language::language_settings::InlineCompletionProvider::Supermaven {
    //     Supermaven::update(cx, |supermaven, cx| supermaven.start(cx));
    // }

    // cx.observe_global::<SettingsStore>(move |cx| {
    //     let new_provider = all_language_settings(None, cx).inline_completions.provider;
    //     if new_provider != provider {
    //         provider = new_provider;
    //         if provider == language::language_settings::InlineCompletionProvider::Supermaven {
    //             Supermaven::update(cx, |supermaven, cx| supermaven.start(cx));
    //         } else {
    //             Supermaven::update(cx, |supermaven, _cx| supermaven.stop());
    //         }
    //     }
    // })
    // .detach();
}

pub enum Supermaven {
    Disabled,
    Started {
        _process: Child,
        next_state_id: SupermavenCompletionStateId,
        states: BTreeMap<SupermavenCompletionStateId, SupermavenCompletionState>,
        outgoing_tx: mpsc::UnboundedSender<OutboundMessage>,
        _handle_outgoing_messages: Task<Result<()>>,
        _handle_incoming_messages: Task<Result<()>>,
    },
}

impl Supermaven {
    pub fn start(&mut self, cx: &mut AppContext) {
        if let Self::Disabled = self {
            cx.spawn(|cx| async move {
                let binary_path = std::env::var("SUPERMAVEN_AGENT_BINARY")
                    .expect("set SUPERMAVEN_AGENT_BINARY env variable");
                let mut process = Command::new(&binary_path)
                    .arg("stdio")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .context("failed to start the binary")?;

                let stdin = process
                    .stdin
                    .take()
                    .context("failed to get stdin for process")?;
                let stdout = process
                    .stdout
                    .take()
                    .context("failed to get stdout for process")?;
                cx.update(|cx| {
                    Self::update(cx, |this, cx| {
                        if let Self::Disabled = this {
                            let (outgoing_tx, outgoing_rx) = mpsc::unbounded();
                            outgoing_tx
                                .unbounded_send(OutboundMessage::UseFreeVersion)
                                .unwrap();
                            *this = Self::Started {
                                _process: process,
                                next_state_id: SupermavenCompletionStateId::default(),
                                states: BTreeMap::default(),
                                outgoing_tx,
                                _handle_outgoing_messages: cx.spawn(|_cx| {
                                    Self::handle_outgoing_messages(outgoing_rx, stdin)
                                }),
                                _handle_incoming_messages: cx
                                    .spawn(|cx| Self::handle_incoming_messages(stdout, cx)),
                            };
                        }
                    });
                })
            })
            .detach_and_log_err(cx);
        }
    }

    pub fn stop(&mut self) {
        *self = Self::Disabled;
    }

    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Started { .. })
    }

    pub fn complete(
        &mut self,
        buffer: &Model<Buffer>,
        cursor_position: Anchor,
        cx: &AppContext,
    ) -> Option<SupermavenCompletion> {
        if let Self::Started {
            next_state_id,
            states,
            outgoing_tx,
            ..
        } = self
        {
            let buffer_id = buffer.entity_id();
            let buffer = buffer.read(cx);
            let path = buffer
                .file()
                .and_then(|file| Some(file.as_local()?.abs_path(cx)))
                .unwrap_or_else(|| PathBuf::from("untitled"))
                .to_string_lossy()
                .to_string();
            let content = buffer.text();
            let offset = cursor_position.to_offset(buffer);
            let state_id = *next_state_id;
            next_state_id.0 += 1;

            let (updates_tx, mut updates_rx) = watch::channel();
            postage::stream::Stream::try_recv(&mut updates_rx).unwrap();

            states.insert(
                state_id,
                SupermavenCompletionState {
                    buffer_id,
                    range: cursor_position.bias_left(buffer)..cursor_position.bias_right(buffer),
                    completion: Vec::new(),
                    text: String::new(),
                    updates_tx,
                },
            );
            let _ = outgoing_tx.unbounded_send(OutboundMessage::StateUpdate(StateUpdateMessage {
                new_id: state_id.0.to_string(),
                updates: vec![
                    StateUpdate::FileUpdate(FileUpdateMessage {
                        path: path.clone(),
                        content,
                    }),
                    StateUpdate::CursorUpdate(CursorPositionUpdateMessage { path, offset }),
                ],
            }));

            Some(SupermavenCompletion {
                id: state_id,
                updates: updates_rx,
            })
        } else {
            None
        }
    }

    pub fn completion(
        &self,
        id: SupermavenCompletionStateId,
    ) -> Option<&SupermavenCompletionState> {
        if let Self::Started { states, .. } = self {
            states.get(&id)
        } else {
            None
        }
    }

    async fn handle_outgoing_messages(
        mut outgoing: mpsc::UnboundedReceiver<OutboundMessage>,
        mut stdin: ChildStdin,
    ) -> Result<()> {
        while let Some(message) = outgoing.next().await {
            let bytes = serde_json::to_vec(&message)?;
            stdin.write_all(&bytes).await?;
            stdin.write_all(&[b'\n']).await?;
        }
        Ok(())
    }

    async fn handle_incoming_messages(stdout: ChildStdout, cx: AsyncAppContext) -> Result<()> {
        const MESSAGE_PREFIX: &str = "SM-MESSAGE ";

        let stdout = BufReader::new(stdout);
        let mut lines = stdout.lines();
        while let Some(line) = lines.next().await {
            let Some(line) = line.context("failed to read line from stdout").log_err() else {
                continue;
            };
            let Some(line) = line.strip_prefix(MESSAGE_PREFIX) else {
                continue;
            };
            let Some(message) = serde_json::from_str::<SupermavenMessage>(&line)
                .with_context(|| format!("failed to deserialize line from stdout: {:?}", line))
                .log_err()
            else {
                continue;
            };

            cx.update(|cx| Self::update(cx, |this, cx| this.handle_message(message, cx)))?;
        }

        Ok(())
    }

    fn handle_message(&mut self, message: SupermavenMessage, cx: &mut AppContext) {
        match message {
            SupermavenMessage::ActivationRequest(request) => {
                let Some(activate_url) = request.activate_url else {
                    return;
                };

                cx.open_window(
                    gpui::WindowOptions {
                        bounds: Some(Bounds::new(
                            gpui::point(DevicePixels::from(0), DevicePixels::from(0)),
                            gpui::size(DevicePixels::from(800), DevicePixels::from(600)),
                        )),
                        titlebar: None,
                        focus: false,
                        ..Default::default()
                    },
                    |cx| cx.new_view(|_cx| ActivationRequestPrompt::new(activate_url.into())),
                );
            }
            SupermavenMessage::Response(response) => {
                if let Self::Started { states, .. } = self {
                    let state_id = SupermavenCompletionStateId(response.state_id.parse().unwrap());
                    if let Some(state) = states.get_mut(&state_id) {
                        for item in &response.items {
                            if let ResponseItem::Text { text } = item {
                                state.text.push_str(text);
                            }
                        }
                        state.completion.extend(response.items);
                        *state.updates_tx.borrow_mut() = ();
                    }
                }
            }
            SupermavenMessage::Passthrough { passthrough } => self.handle_message(*passthrough, cx),
            _ => {
                dbg!(&message);
            }
        }
    }
}

impl Global for Supermaven {}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct SupermavenCompletionStateId(usize);

#[allow(dead_code)]
pub struct SupermavenCompletionState {
    buffer_id: EntityId,
    range: Range<Anchor>,
    completion: Vec<ResponseItem>,
    text: String,
    updates_tx: watch::Sender<()>,
}

pub struct SupermavenCompletion {
    pub id: SupermavenCompletionStateId,
    pub updates: watch::Receiver<()>,
}

struct ActivationRequestPrompt {
    activate_url: SharedString,
}

impl ActivationRequestPrompt {
    fn new(activate_url: SharedString) -> Self {
        Self { activate_url }
    }
}

impl Render for ActivationRequestPrompt {
    fn render(&mut self, _cx: &mut ViewContext<Self>) -> impl gpui::prelude::IntoElement {
        InteractiveText::new(
            "activation_prompt",
            StyledText::new(self.activate_url.clone()),
        )
        .on_click(vec![0..self.activate_url.len()], {
            let activate_url = self.activate_url.clone();
            move |_, cx| cx.open_url(&activate_url)
        })
    }
}

// #[cfg(test)]
// mod tests {
//     use std::sync::Arc;

//     use super::*;
//     use gpui::{Context, TestAppContext};
//     use language::{language_settings::AllLanguageSettings, Buffer, BufferId, LanguageRegistry};
//     use project::Project;
//     use settings::SettingsStore;

//     #[gpui::test]
//     async fn test_exploratory(cx: &mut TestAppContext) {
//         env_logger::init();

//         init_test(cx);
//         let background_executor = cx.executor();
//         background_executor.allow_parking();

//         cx.update(Supermaven::launch).await.unwrap();

//         let language_registry = Arc::new(LanguageRegistry::test(background_executor.clone()));

//         let python = language_registry.language_for_name("Python");

//         let buffer = cx.new_model(|cx| {
//             let mut buffer = Buffer::new(0, BufferId::new(cx.entity_id().as_u64()).unwrap(), "");
//             buffer.set_language_registry(language_registry);
//             cx.spawn(|buffer, mut cx| async move {
//                 let python = python.await?;
//                 buffer.update(&mut cx, |buffer: &mut Buffer, cx| {
//                     buffer.set_language(Some(python), cx);
//                 })?;
//                 anyhow::Ok(())
//             })
//             .detach_and_log_err(cx);
//             buffer
//         });

//         let editor = cx.add_window(|cx| Editor::for_buffer(buffer, None, cx));
//         editor
//             .update(cx, |editor, cx| editor.insert("import numpy as ", cx))
//             .unwrap();

//         cx.executor()
//             .timer(std::time::Duration::from_secs(60))
//             .await;
//     }

//     pub fn init_test(cx: &mut TestAppContext) {
//         _ = cx.update(|cx| {
//             let store = SettingsStore::test(cx);
//             cx.set_global(store);
//             theme::init(theme::LoadThemes::JustBase, cx);
//             language::init(cx);
//             Project::init_settings(cx);
//             editor::init(cx);
//             SettingsStore::update(cx, |store, cx| {
//                 store.update_user_settings::<AllLanguageSettings>(cx, |_| {});
//             });
//         });
//     }
// }
