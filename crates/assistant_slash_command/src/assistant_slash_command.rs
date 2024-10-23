mod slash_command_registry;

use anyhow::Result;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use gpui::{AnyElement, AppContext, ElementId, SharedString, Task, WeakView, WindowContext};
use language::{BufferSnapshot, CodeLabel, LspAdapterDelegate, OffsetRangeExt};
use serde::{Deserialize, Serialize};
pub use slash_command_registry::*;
use std::{
    ops::Range,
    sync::{atomic::AtomicBool, Arc},
};
use workspace::{ui::IconName, Workspace};

pub fn init(cx: &mut AppContext) {
    SlashCommandRegistry::default_global(cx);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AfterCompletion {
    /// Run the command
    Run,
    /// Continue composing the current argument, doesn't add a space
    Compose,
    /// Continue the command composition, adds a space
    Continue,
}

impl From<bool> for AfterCompletion {
    fn from(value: bool) -> Self {
        if value {
            AfterCompletion::Run
        } else {
            AfterCompletion::Continue
        }
    }
}

impl AfterCompletion {
    pub fn run(&self) -> bool {
        match self {
            AfterCompletion::Run => true,
            AfterCompletion::Compose | AfterCompletion::Continue => false,
        }
    }
}

#[derive(Debug)]
pub struct ArgumentCompletion {
    /// The label to display for this completion.
    pub label: CodeLabel,
    /// The new text that should be inserted into the command when this completion is accepted.
    pub new_text: String,
    /// Whether the command should be run when accepting this completion.
    pub after_completion: AfterCompletion,
    /// Whether to replace the all arguments, or whether to treat this as an independent argument.
    pub replace_previous_arguments: bool,
}

pub type SlashCommandResult = Result<BoxStream<'static, Result<SlashCommandEvent>>>;

pub trait SlashCommand: 'static + Send + Sync {
    fn name(&self) -> String;
    fn label(&self, _cx: &AppContext) -> CodeLabel {
        CodeLabel::plain(self.name(), None)
    }
    fn description(&self) -> String;
    fn menu_text(&self) -> String;
    fn complete_argument(
        self: Arc<Self>,
        arguments: &[String],
        cancel: Arc<AtomicBool>,
        workspace: Option<WeakView<Workspace>>,
        cx: &mut WindowContext,
    ) -> Task<Result<Vec<ArgumentCompletion>>>;
    fn requires_argument(&self) -> bool;
    fn accepts_arguments(&self) -> bool {
        self.requires_argument()
    }
    fn run(
        self: Arc<Self>,
        arguments: &[String],
        context_slash_command_output_sections: &[SlashCommandOutputSection<language::Anchor>],
        context_buffer: BufferSnapshot,
        workspace: WeakView<Workspace>,
        // TODO: We're just using the `LspAdapterDelegate` here because that is
        // what the extension API is already expecting.
        //
        // It may be that `LspAdapterDelegate` needs a more general name, or
        // perhaps another kind of delegate is needed here.
        delegate: Option<Arc<dyn LspAdapterDelegate>>,
        cx: &mut WindowContext,
    ) -> Task<SlashCommandResult>;
}

pub type RenderFoldPlaceholder = Arc<
    dyn Send
        + Sync
        + Fn(ElementId, Arc<dyn Fn(&mut WindowContext)>, &mut WindowContext) -> AnyElement,
>;

#[derive(Debug, PartialEq, Eq)]
pub enum SlashCommandContent {
    Text {
        text: String,
        run_commands_in_text: bool,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum SlashCommandEvent {
    StartSection {
        icon: IconName,
        label: SharedString,
        metadata: Option<serde_json::Value>,
    },
    Content(SlashCommandContent),
    EndSection {
        metadata: Option<serde_json::Value>,
    },
}

#[derive(Debug, Default, PartialEq, Clone)]
pub struct SlashCommandOutput {
    pub text: String,
    pub sections: Vec<SlashCommandOutputSection<usize>>,
    pub run_commands_in_text: bool,
}

impl SlashCommandOutput {
    /// Returns this [`SlashCommandOutput`] as a stream of [`SlashCommandEvent`]s.
    pub fn to_event_stream(self) -> BoxStream<'static, Result<SlashCommandEvent>> {
        let mut events = Vec::new();
        let mut last_section_end = 0;

        for section in self.sections {
            if last_section_end < section.range.start {
                events.push(Ok(SlashCommandEvent::Content(SlashCommandContent::Text {
                    text: self
                        .text
                        .get(last_section_end..section.range.start)
                        .unwrap_or_default()
                        .to_string(),
                    run_commands_in_text: self.run_commands_in_text,
                })));
            }

            events.push(Ok(SlashCommandEvent::StartSection {
                icon: section.icon,
                label: section.label,
                metadata: section.metadata.clone(),
            }));
            events.push(Ok(SlashCommandEvent::Content(SlashCommandContent::Text {
                text: self
                    .text
                    .get(section.range.start..section.range.end)
                    .unwrap_or_default()
                    .to_string(),
                run_commands_in_text: self.run_commands_in_text,
            })));
            events.push(Ok(SlashCommandEvent::EndSection {
                metadata: section.metadata,
            }));

            last_section_end = section.range.end;
        }

        if last_section_end < self.text.len() {
            events.push(Ok(SlashCommandEvent::Content(SlashCommandContent::Text {
                text: self.text[last_section_end..].to_string(),
                run_commands_in_text: self.run_commands_in_text,
            })));
        }

        stream::iter(events).boxed()
    }

    pub async fn from_event_stream(
        mut events: BoxStream<'static, Result<SlashCommandEvent>>,
    ) -> Result<SlashCommandOutput> {
        let mut output = SlashCommandOutput::default();
        let mut current_section = None;

        while let Some(event) = events.next().await {
            match event? {
                SlashCommandEvent::StartSection {
                    icon,
                    label,
                    metadata,
                } => {
                    if let Some(section) = current_section.take() {
                        output.sections.push(section);
                    }

                    let start = output.text.len();
                    current_section = Some(SlashCommandOutputSection {
                        range: start..start,
                        icon,
                        label,
                        metadata,
                    });
                }
                SlashCommandEvent::Content(SlashCommandContent::Text {
                    text,
                    run_commands_in_text,
                }) => {
                    output.text.push_str(&text);
                    output.run_commands_in_text = run_commands_in_text;

                    if let Some(section) = current_section.as_mut() {
                        section.range.end = output.text.len();
                    }
                }
                SlashCommandEvent::EndSection { metadata } => {
                    if let Some(mut section) = current_section.take() {
                        section.metadata = metadata;
                        output.sections.push(section);
                    }
                }
            }
        }

        if let Some(section) = current_section {
            output.sections.push(section);
        }

        Ok(output)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommandOutputSection<T> {
    pub range: Range<T>,
    pub icon: IconName,
    pub label: SharedString,
    pub metadata: Option<serde_json::Value>,
}

impl SlashCommandOutputSection<language::Anchor> {
    pub fn is_valid(&self, buffer: &language::TextBuffer) -> bool {
        self.range.start.is_valid(buffer) && !self.range.to_offset(buffer).is_empty()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[gpui::test]
    async fn test_slash_command_output_to_events_round_trip() {
        // Test basic output consisting of a single section.
        {
            let text = "Hello, world!".to_string();
            let range = 0..text.len();
            let output = SlashCommandOutput {
                text,
                sections: vec![SlashCommandOutputSection {
                    range,
                    icon: IconName::Code,
                    label: "Section 1".into(),
                    metadata: None,
                }],
                run_commands_in_text: false,
            };

            let events = output.clone().to_event_stream().collect::<Vec<_>>().await;
            let events = events
                .into_iter()
                .filter_map(|event| event.ok())
                .collect::<Vec<_>>();

            assert_eq!(
                events,
                vec![
                    SlashCommandEvent::StartSection {
                        icon: IconName::Code,
                        label: "Section 1".into(),
                        metadata: None
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "Hello, world!".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::EndSection { metadata: None }
                ]
            );

            let new_output =
                SlashCommandOutput::from_event_stream(output.clone().to_event_stream())
                    .await
                    .unwrap();

            assert_eq!(new_output, output);
        }

        // Test output where the sections do not comprise all of the text.
        {
            let text = "Apple\nCucumber\nBanana\n".to_string();
            let output = SlashCommandOutput {
                text,
                sections: vec![
                    SlashCommandOutputSection {
                        range: 0..6,
                        icon: IconName::Check,
                        label: "Fruit".into(),
                        metadata: None,
                    },
                    SlashCommandOutputSection {
                        range: 15..22,
                        icon: IconName::Check,
                        label: "Fruit".into(),
                        metadata: None,
                    },
                ],
                run_commands_in_text: false,
            };

            let events = output.clone().to_event_stream().collect::<Vec<_>>().await;
            let events = events
                .into_iter()
                .filter_map(|event| event.ok())
                .collect::<Vec<_>>();

            assert_eq!(
                events,
                vec![
                    SlashCommandEvent::StartSection {
                        icon: IconName::Check,
                        label: "Fruit".into(),
                        metadata: None
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "Apple\n".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::EndSection { metadata: None },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "Cucumber\n".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::StartSection {
                        icon: IconName::Check,
                        label: "Fruit".into(),
                        metadata: None
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "Banana\n".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::EndSection { metadata: None }
                ]
            );

            let new_output =
                SlashCommandOutput::from_event_stream(output.clone().to_event_stream())
                    .await
                    .unwrap();

            assert_eq!(new_output, output);
        }

        // Test output consisting of multiple sections.
        {
            let text = "Line 1\nLine 2\nLine 3\nLine 4\n".to_string();
            let output = SlashCommandOutput {
                text,
                sections: vec![
                    SlashCommandOutputSection {
                        range: 0..6,
                        icon: IconName::FileCode,
                        label: "Section 1".into(),
                        metadata: Some(json!({ "a": true })),
                    },
                    SlashCommandOutputSection {
                        range: 7..13,
                        icon: IconName::FileDoc,
                        label: "Section 2".into(),
                        metadata: Some(json!({ "b": true })),
                    },
                    SlashCommandOutputSection {
                        range: 14..20,
                        icon: IconName::FileGit,
                        label: "Section 3".into(),
                        metadata: Some(json!({ "c": true })),
                    },
                    SlashCommandOutputSection {
                        range: 21..27,
                        icon: IconName::FileToml,
                        label: "Section 4".into(),
                        metadata: Some(json!({ "d": true })),
                    },
                ],
                run_commands_in_text: false,
            };

            let events = output.clone().to_event_stream().collect::<Vec<_>>().await;
            let events = events
                .into_iter()
                .filter_map(|event| event.ok())
                .collect::<Vec<_>>();

            assert_eq!(
                events,
                vec![
                    SlashCommandEvent::StartSection {
                        icon: IconName::FileCode,
                        label: "Section 1".into(),
                        metadata: Some(json!({ "a": true }))
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "Line 1".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::EndSection {
                        metadata: Some(json!({ "a": true }))
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "\n".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::StartSection {
                        icon: IconName::FileDoc,
                        label: "Section 2".into(),
                        metadata: Some(json!({ "b": true }))
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "Line 2".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::EndSection {
                        metadata: Some(json!({ "b": true }))
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "\n".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::StartSection {
                        icon: IconName::FileGit,
                        label: "Section 3".into(),
                        metadata: Some(json!({ "c": true }))
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "Line 3".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::EndSection {
                        metadata: Some(json!({ "c": true }))
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "\n".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::StartSection {
                        icon: IconName::FileToml,
                        label: "Section 4".into(),
                        metadata: Some(json!({ "d": true }))
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "Line 4".into(),
                        run_commands_in_text: false
                    }),
                    SlashCommandEvent::EndSection {
                        metadata: Some(json!({ "d": true }))
                    },
                    SlashCommandEvent::Content(SlashCommandContent::Text {
                        text: "\n".into(),
                        run_commands_in_text: false
                    }),
                ]
            );

            let new_output =
                SlashCommandOutput::from_event_stream(output.clone().to_event_stream())
                    .await
                    .unwrap();

            assert_eq!(new_output, output);
        }
    }
}
