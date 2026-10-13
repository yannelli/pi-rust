//! AWS MFA codes for assume-role profiles with `mfa_serial`: while the interactive mode runs,
//! the credential chain asks for the code in an input dialog.

use std::sync::Arc;

use pi_ai::{MfaCodeProvider, MfaPrompt};
use tokio::sync::{mpsc, oneshot};

use super::mode::InteractiveMode;

type MfaRequest = (MfaPrompt, oneshot::Sender<Result<String, String>>);

/// The input title for an MFA prompt.
pub(super) fn mfa_prompt_title(prompt: &MfaPrompt) -> String {
    format!("AWS MFA code for profile \"{}\" ({})", prompt.profile, prompt.serial)
}

impl InteractiveMode {
    /// Install the process-wide AWS MFA callback; requests are answered on the UI thread.
    pub(super) fn install_aws_mfa_prompt(&self) {
        let (sender, mut requests) = mpsc::unbounded_channel::<MfaRequest>();
        let provider: MfaCodeProvider = Arc::new(move |prompt| {
            let (reply, answer) = oneshot::channel();
            let sent = sender.send((prompt, reply)).is_ok();
            Box::pin(async move {
                if !sent {
                    return Err("MFA prompt is not available".to_string());
                }
                answer.await.unwrap_or_else(|_| Err("MFA prompt closed".to_string()))
            })
        });
        pi_ai::set_mfa_code_provider(Some(provider));
        self.spawn(|mode| async move {
            let weak = mode.weak.clone();
            drop(mode);
            while let Some((prompt, reply)) = requests.recv().await {
                let Some(mode) = weak.upgrade() else { break };
                let answer = mode.ask_input_until(&mfa_prompt_title(&prompt), None, None);
                drop(mode);
                let code = answer.await.ok().flatten().map(|code| code.trim().to_string());
                let _ =
                    reply.send(code.filter(|code| !code.is_empty()).ok_or_else(|| "MFA code entry cancelled".into()));
            }
        });
    }

    /// Remove the MFA callback when the mode stops.
    pub(super) fn remove_aws_mfa_prompt(&self) {
        pi_ai::set_mfa_code_provider(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_the_profile_and_device() {
        let prompt = MfaPrompt { profile: "prod".into(), serial: "arn:aws:iam::1:mfa/me".into() };
        assert_eq!(mfa_prompt_title(&prompt), "AWS MFA code for profile \"prod\" (arn:aws:iam::1:mfa/me)");
    }
}
