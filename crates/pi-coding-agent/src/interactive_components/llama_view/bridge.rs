//! [`LlamaViewUi`]: the `Send + Sync` [`LlamaUi`] over a [`LlamaView`]. Calls travel over a
//! channel to a `spawn_local` task on the UI thread, which updates the view and replies.

use std::cell::RefCell;
use std::rc::Rc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use super::{LlamaView, hugging_face_search_fn};
use crate::llama::view::ProgressState;
use crate::llama::{ConnectionChoice, HuggingFaceClient, LlamaManagerAction, LlamaModelInfo, LlamaUi, NotifyLevel};

enum Request {
    Models(String, Vec<LlamaModelInfo>, oneshot::Sender<LlamaManagerAction>),
    Select(String, Vec<String>, oneshot::Sender<Option<String>>),
    Search(HuggingFaceClient, oneshot::Sender<Option<String>>),
    Status(String, String),
    Progress(ProgressState, oneshot::Sender<()>),
    UpdateProgress(ProgressState),
    Notify(String, NotifyLevel),
}

/// The [`LlamaUi`] of a mounted [`LlamaView`]; build it with [`LlamaView::bridge`]. Once the
/// view is dropped, screens resolve as closed or cancelled.
#[derive(Clone)]
pub struct LlamaViewUi {
    sender: mpsc::UnboundedSender<Request>,
}

impl LlamaView {
    /// Serves [`LlamaUi`] calls from any thread on a `spawn_local` task (needs a `LocalSet`);
    /// `on_notify` receives [`LlamaUi::notify`] messages.
    pub fn bridge(
        view: &Rc<RefCell<LlamaView>>,
        mut on_notify: impl FnMut(&str, NotifyLevel) + 'static,
    ) -> LlamaViewUi {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let view = Rc::downgrade(view);
        tokio::task::spawn_local(async move {
            while let Some(request) = receiver.recv().await {
                if let Request::Notify(message, level) = &request {
                    on_notify(message, *level);
                    continue;
                }
                let Some(view) = view.upgrade() else { break };
                let mut view = view.borrow_mut();
                match request {
                    Request::Models(url, models, reply) => {
                        view.show_models(&url, &models, move |action| {
                            let _ = reply.send(action);
                        });
                    }
                    Request::Select(title, options, reply) => {
                        view.select(&title, &options, move |choice| {
                            let _ = reply.send(choice);
                        });
                    }
                    Request::Search(client, reply) => {
                        view.search_models(hugging_face_search_fn(client), move |model| {
                            let _ = reply.send(model);
                        });
                    }
                    Request::Status(title, message) => view.show_status(&title, &message),
                    Request::Progress(state, reply) => view.progress(&state, move || {
                        let _ = reply.send(());
                    }),
                    Request::UpdateProgress(state) => view.update_progress(&state),
                    Request::Notify(..) => {}
                }
            }
        });
        LlamaViewUi { sender }
    }
}

impl LlamaViewUi {
    fn ask<T>(&self, request: impl FnOnce(oneshot::Sender<T>) -> Request) -> oneshot::Receiver<T> {
        let (reply, receiver) = oneshot::channel();
        let _ = self.sender.send(request(reply));
        receiver
    }
}

#[async_trait]
impl LlamaUi for LlamaViewUi {
    async fn show_models(&self, server_url: &str, models: &[LlamaModelInfo]) -> LlamaManagerAction {
        let receiver = self.ask(|reply| Request::Models(server_url.into(), models.to_vec(), reply));
        receiver.await.unwrap_or(LlamaManagerAction::Close)
    }

    async fn select(&self, title: &str, options: &[String]) -> Option<String> {
        self.ask(|reply| Request::Select(title.into(), options.to_vec(), reply)).await.ok().flatten()
    }

    async fn confirm(&self, title: &str, message: &str) -> bool {
        let options = ["Yes", "No"].map(String::from);
        self.select(&format!("{title}\n{message}"), &options).await.as_deref() == Some("Yes")
    }

    async fn connection_error(&self, server_url: &str, message: &str) -> ConnectionChoice {
        let title = format!("llama.cpp unavailable\n{server_url}\n\n{message}");
        match self.select(&title, &["Retry", "Close"].map(String::from)).await.as_deref() {
            Some("Retry") => ConnectionChoice::Retry,
            _ => ConnectionChoice::Close,
        }
    }

    async fn search_models(&self, client: &HuggingFaceClient) -> Option<String> {
        self.ask(|reply| Request::Search(client.clone(), reply)).await.ok().flatten()
    }

    fn show_status(&self, title: &str, message: &str) {
        let _ = self.sender.send(Request::Status(title.into(), message.into()));
    }

    async fn progress(&self, state: &ProgressState) {
        // A screen that replaced the progress view drops the reply; the stop never comes.
        if self.ask(|reply| Request::Progress(state.clone(), reply)).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    fn update_progress(&self, state: &ProgressState) {
        let _ = self.sender.send(Request::UpdateProgress(state.clone()));
    }

    fn notify(&self, message: &str, level: NotifyLevel) {
        let _ = self.sender.send(Request::Notify(message.into(), level));
    }
}
