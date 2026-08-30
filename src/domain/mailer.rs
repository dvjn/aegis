use async_trait::async_trait;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::header::ContentType,
    transport::smtp::authentication::{Credentials, Mechanism},
};

use super::error::DomainError;

pub struct Mail {
    pub to: String,
    pub subject: String,
    pub body: String,
}

#[async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, mail: Mail) -> Result<(), DomainError>;
}

pub struct LogMailer;

#[async_trait]
impl Mailer for LogMailer {
    async fn send(&self, mail: Mail) -> Result<(), DomainError> {
        tracing::info!(
            target: "aegis::mail",
            to = %mail.to,
            subject = %mail.subject,
            "MAIL NOT SENT: no SMTP server is configured; body={:?}",
            mail.body
        );
        Ok(())
    }
}

#[derive(Clone)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub from: String,
}

pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: lettre::message::Mailbox,
}

impl SmtpMailer {
    pub fn new(settings: SmtpSettings) -> Result<Self, DomainError> {
        let from = settings
            .from
            .parse()
            .map_err(|_| DomainError::InvalidInput("invalid SMTP from address"))?;
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&settings.host)
            .map_err(|_| DomainError::InvalidInput("invalid SMTP host"))?
            .port(settings.port);
        if let (Some(username), Some(password)) = (settings.username, settings.password) {
            builder = builder
                .credentials(Credentials::new(username, password))
                .authentication(vec![Mechanism::Plain, Mechanism::Login]);
        }
        Ok(Self {
            transport: builder.build(),
            from,
        })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, mail: Mail) -> Result<(), DomainError> {
        let to = mail
            .to
            .parse()
            .map_err(|_| DomainError::InvalidInput("invalid recipient address"))?;
        let message = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(mail.subject)
            .header(ContentType::TEXT_PLAIN)
            .body(mail.body)
            .map_err(|_| DomainError::InvalidInput("invalid message"))?;
        self.transport.send(message).await.map_err(|error| {
            tracing::error!(%error, "failed to send mail");
            DomainError::TemporarilyUnavailable
        })?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct CapturingMailer(Mutex<Vec<Mail>>);

    impl CapturingMailer {
        pub(crate) fn take(&self) -> Vec<Mail> {
            std::mem::take(&mut self.0.lock().expect("mail lock"))
        }
    }

    #[async_trait]
    impl Mailer for CapturingMailer {
        async fn send(&self, mail: Mail) -> Result<(), DomainError> {
            self.0.lock().expect("mail lock").push(mail);
            Ok(())
        }
    }

    pub(crate) fn extract_code(body: &str) -> String {
        body.split_whitespace()
            .map(|word| word.trim_matches(|item: char| !item.is_ascii_digit()))
            .find(|word| word.len() == 6 && word.chars().all(|item| item.is_ascii_digit()))
            .expect("mail body carries a one-time code")
            .to_owned()
    }
}
