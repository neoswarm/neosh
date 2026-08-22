//! Options that are spelled into the message rather than sent beside it.
//!
//! # Why any of this exists
//!
//! Not every knob a vendor offers is a request parameter. A harness that wraps a model — `claude`,
//! and every agent CLI that has copied it — reads words out of the prompt and changes how it runs:
//! `ultrathink` buys thinking depth past anything `--effort` will accept, `ultracode` turns on
//! orchestration that has no flag at all. They are settings by every test that matters — you choose
//! one, it changes the next turn, and it should be visible in the strip while it is on — and they
//! are settings you cannot *send*.
//!
//! [`ProviderOptionDescriptor`] has carried `prompt_injected_values` since ADR 0011 and nothing has
//! ever acted on it, which meant choosing one sent the word as a parameter: `--effort ultrathink`,
//! which is not a level, and the turn fails. This is the half that was missing.
//!
//! # Where it happens, and why not in the driver
//!
//! Above every driver, in the agent, on the request rather than on the conversation. Three
//! consequences, all of them the point:
//!
//! * **Every driver gets it.** A vendor plugin that advertises a magic word needs no code here.
//! * **The transcript never sees it.** The word goes into the copy of the message that this turn
//!   sends. What you typed is what stays on screen and what the next turn replays — otherwise every
//!   turn after an ultrathink would carry the word again, and turning it off would not turn it off.
//! * **Nothing invalid reaches the wire.** The injected value is replaced with the ladder's ordinary
//!   default, so the parameter is always one the endpoint accepts.

use neosh_proto::{
    Message, ModelInfo, ModelSelection, ProviderOptionDescriptor, ProviderOptionValue, Role,
};

/// The words a selection asks for, with the injected values taken back out of it.
///
/// Mutates `selection` so what is left is safe to send: a spelled level becomes the ladder's
/// default, and a spelled flag becomes nothing at all.
///
/// `models` is the catalogue for the instance being used. A model nothing here describes has no
/// descriptors, so it has no magic words, so this does nothing — which is the right answer for
/// every discovered endpoint that told us only an id.
pub fn prompt_injections(models: &[ModelInfo], selection: &mut ModelSelection) -> Vec<String> {
    let Some(model) = models.iter().find(|m| m.id == selection.model) else {
        return Vec::new();
    };

    let mut words = Vec::new();
    for descriptor in &model.capabilities.option_descriptors {
        match descriptor {
            ProviderOptionDescriptor::Select {
                id, options, prompt_injected_values, ..
            } if !prompt_injected_values.is_empty() => {
                let Some(chosen) = selection.option_str(id).map(str::to_string) else { continue };
                if !prompt_injected_values.contains(&chosen) {
                    continue;
                }
                words.push(chosen);
                // Back to the top of the ladder that can actually be sent. Dropping the option
                // instead would hand the endpoint its own default, which on a driver whose default
                // is "medium" would quietly *lower* the depth of the turn you asked to deepen.
                let fallback = options
                    .iter()
                    .filter(|o| !prompt_injected_values.contains(&o.id))
                    .find(|o| o.is_default)
                    .or_else(|| {
                        options.iter().rfind(|o| !prompt_injected_values.contains(&o.id))
                    })
                    .map(|o| o.id.clone());
                match fallback {
                    Some(v) => set(selection, id, ProviderOptionValue::Text(v)),
                    None => selection.options.retain(|o| &o.id != id),
                }
            }
            ProviderOptionDescriptor::Boolean { id, prompt_injected_word: Some(word), .. } => {
                if selection.option_flag(id) == Some(true) {
                    words.push(word.clone());
                }
                // Off *and* on: the switch is the word, so there is no parameter either way, and
                // sending `{"ultracode": true}` to an endpoint that has never heard of it is how a
                // knob that works becomes a 400.
                selection.options.retain(|o| &o.id != id);
            }
            _ => {}
        }
    }
    words
}

fn set(selection: &mut ModelSelection, id: &str, value: ProviderOptionValue) {
    match selection.options.iter_mut().find(|o| o.id == id) {
        Some(slot) => slot.value = value,
        None => selection.options.push(neosh_proto::OptionSelection { id: id.into(), value }),
    }
}

/// Put the words at the top of the message this turn is about to send.
///
/// The **last user message**, because that is the one being answered: a word attached to the
/// question from four turns ago is a word the model reads as history. Each on its own line and in
/// the order the descriptors declared them, which is the order they appear in the picker.
///
/// A turn with no user message in it — a tool result being handed back mid-loop — is left alone. The
/// word was already said on the message that started the loop, and saying it again on every round
/// trip would be a different instruction each time the model looked.
pub fn inject(messages: &mut [Message], words: &[String]) {
    if words.is_empty() {
        return;
    }
    let Some(last) = messages.iter_mut().rfind(|m| m.role == Role::User) else { return };
    let prefix = format!("{}\n\n", words.join("\n"));
    // The first text block, not a new one: a message whose first block is an image and whose second
    // is the question would otherwise get the word before the image, where a vendor scanning "the
    // text of the prompt" may not look at all.
    for block in last.content.iter_mut() {
        if let neosh_proto::ContentBlock::Text { text } = block {
            *text = format!("{prefix}{text}");
            return;
        }
    }
    last.content.insert(0, neosh_proto::ContentBlock::Text { text: words.join("\n") });
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::{
        ContentBlock, ModelCapabilities, OptionChoice, OptionSelection, ProviderOptionDescriptor,
    };

    fn ladder() -> ProviderOptionDescriptor {
        ProviderOptionDescriptor::Select {
            id: "effort".into(),
            label: "Effort".into(),
            description: None,
            options: vec![
                OptionChoice { id: "low".into(), label: "Low".into(), description: None, is_default: false },
                OptionChoice { id: "high".into(), label: "High".into(), description: None, is_default: true },
                OptionChoice { id: "ultrathink".into(), label: "Ultrathink".into(), description: None, is_default: false },
            ],
            current_value: Some("high".into()),
            prompt_injected_values: vec!["ultrathink".into()],
        }
    }

    fn switch() -> ProviderOptionDescriptor {
        ProviderOptionDescriptor::Boolean {
            id: "ultracode".into(),
            label: "Ultracode".into(),
            description: None,
            current_value: false,
            prompt_injected_word: Some("ultracode".into()),
        }
    }

    fn model() -> Vec<ModelInfo> {
        let mut m = ModelInfo::undescribed("m", "M");
        m.capabilities = ModelCapabilities {
            option_descriptors: vec![ladder(), switch()],
            ..ModelCapabilities::default()
        };
        vec![m]
    }

    fn selection(options: Vec<OptionSelection>) -> ModelSelection {
        ModelSelection { instance: "i".into(), model: "m".into(), options }
    }

    fn text(id: &str, v: &str) -> OptionSelection {
        OptionSelection { id: id.into(), value: ProviderOptionValue::Text(v.into()) }
    }

    #[test]
    fn a_spelled_level_becomes_a_word_and_a_sendable_value() {
        // `--effort ultrathink` is not a level. The whole reason this exists is that choosing one
        // used to put it on the command line.
        let mut sel = selection(vec![text("effort", "ultrathink")]);
        assert_eq!(prompt_injections(&model(), &mut sel), vec!["ultrathink".to_string()]);
        assert_eq!(sel.option_str("effort"), Some("high"), "the ladder's own default, not the endpoint's");
    }

    #[test]
    fn an_ordinary_level_is_left_exactly_where_it_was() {
        let mut sel = selection(vec![text("effort", "low")]);
        assert!(prompt_injections(&model(), &mut sel).is_empty());
        assert_eq!(sel.option_str("effort"), Some("low"));
    }

    #[test]
    fn a_spelled_switch_sends_nothing_either_way() {
        for on in [true, false] {
            let mut sel = selection(vec![OptionSelection {
                id: "ultracode".into(),
                value: ProviderOptionValue::Flag(on),
            }]);
            let words = prompt_injections(&model(), &mut sel);
            assert_eq!(words.is_empty(), !on);
            assert!(sel.option_flag("ultracode").is_none(), "the switch is the word, not a parameter");
        }
    }

    #[test]
    fn a_model_nobody_described_has_no_magic_words() {
        let mut sel = ModelSelection {
            instance: "i".into(),
            model: "something-discovered".into(),
            options: vec![text("effort", "ultrathink")],
        };
        assert!(prompt_injections(&model(), &mut sel).is_empty());
        assert_eq!(sel.option_str("effort"), Some("ultrathink"), "untouched, because unknown");
    }

    #[test]
    fn the_word_goes_on_the_question_being_answered() {
        let mut messages = vec![
            Message { role: Role::User, content: vec![ContentBlock::Text { text: "first".into() }] },
            Message { role: Role::Assistant, content: vec![ContentBlock::Text { text: "answer".into() }] },
            Message { role: Role::User, content: vec![ContentBlock::Text { text: "second".into() }] },
        ];
        inject(&mut messages, &["ultrathink".into()]);
        let ContentBlock::Text { text: first } = &messages[0].content[0] else { panic!() };
        let ContentBlock::Text { text: last } = &messages[2].content[0] else { panic!() };
        assert_eq!(first, "first", "history is what was actually said");
        assert_eq!(last, "ultrathink\n\nsecond");
    }

    #[test]
    fn nothing_chosen_rewrites_nothing() {
        let mut messages =
            vec![Message { role: Role::User, content: vec![ContentBlock::Text { text: "hi".into() }] }];
        inject(&mut messages, &[]);
        let ContentBlock::Text { text } = &messages[0].content[0] else { panic!() };
        assert_eq!(text, "hi");
    }
}
