//! The questions `srtty setup` asks, behind a trait so the flow can be tested
//! without a terminal.

use std::io::Write;

use inquire::error::InquireError;
use inquire::validator::Validation;
use inquire::{Confirm, Select, Text};

use crate::error::SrttyError;

/// Asks the user. Escape and Ctrl-C yield [`SrttyError::Interrupted`].
pub trait Prompter {
    /// Index of the chosen option.
    fn select(&mut self, message: &str, options: &[String]) -> Result<usize, SrttyError>;
    /// A line of text accepted by `validate`, whose error is shown to the user.
    fn text(
        &mut self,
        message: &str,
        default: Option<&str>,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<String, SrttyError>;
    fn confirm(&mut self, message: &str, default: bool) -> Result<bool, SrttyError>;
    /// Shows a line of information.
    fn say(&mut self, line: &str);
}

/// Prompts on the terminal with `inquire`, which draws on stderr.
pub struct InquirePrompter;

impl Prompter for InquirePrompter {
    fn select(&mut self, message: &str, options: &[String]) -> Result<usize, SrttyError> {
        Select::new(message, options.to_vec())
            .without_filtering()
            .raw_prompt()
            .map(|choice| choice.index)
            .map_err(prompt_error)
    }

    fn text(
        &mut self,
        message: &str,
        default: Option<&str>,
        validate: &dyn Fn(&str) -> Result<(), String>,
    ) -> Result<String, SrttyError> {
        let mut prompt = Text::new(message).with_validator(move |input: &str| {
            Ok(match validate(input) {
                Ok(()) => Validation::Valid,
                Err(msg) => Validation::Invalid(msg.into()),
            })
        });
        if let Some(default) = default {
            prompt = prompt.with_default(default);
        }
        prompt.prompt().map_err(prompt_error)
    }

    fn confirm(&mut self, message: &str, default: bool) -> Result<bool, SrttyError> {
        Confirm::new(message)
            .with_default(default)
            .prompt()
            .map_err(prompt_error)
    }

    fn say(&mut self, line: &str) {
        let _ = writeln!(std::io::stderr().lock(), "{line}");
    }
}

fn prompt_error(e: InquireError) -> SrttyError {
    match e {
        InquireError::OperationCanceled | InquireError::OperationInterrupted => {
            SrttyError::Interrupted
        }
        InquireError::NotTTY => {
            SrttyError::Usage("`srtty setup` is interactive; run it in a terminal".into())
        }
        InquireError::IO(e) => SrttyError::Io(e),
        other => SrttyError::Usage(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_and_ctrl_c_are_interruptions() {
        for e in [
            InquireError::OperationCanceled,
            InquireError::OperationInterrupted,
        ] {
            assert_eq!(prompt_error(e).exit_code(), 130);
        }
        assert!(matches!(
            prompt_error(InquireError::NotTTY),
            SrttyError::Usage(m) if m.contains("interactive")
        ));
    }
}
