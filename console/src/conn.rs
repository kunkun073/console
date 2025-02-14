use console_api::tasks::{
    tasks_client::TasksClient, DetailsRequest, TaskDetails, TaskUpdate, TasksRequest,
};
use futures::stream::StreamExt;
use std::{error::Error, pin::Pin, time::Duration};
use tonic::{transport::Channel, transport::Uri, Streaming};

#[derive(Debug)]
pub struct Connection {
    target: Uri,
    state: State,
}

#[derive(Debug)]
enum State {
    Connected {
        client: TasksClient<Channel>,
        stream: Streaming<TaskUpdate>,
    },
    Disconnected(Duration),
}

impl Connection {
    const BACKOFF: Duration = Duration::from_millis(500);
    pub fn new(target: Uri) -> Self {
        Self {
            target,
            state: State::Disconnected(Duration::from_secs(0)),
        }
    }

    async fn connect(&mut self) {
        const MAX_BACKOFF: Duration = Duration::from_secs(5);

        while let State::Disconnected(backoff) = self.state {
            if backoff == Duration::from_secs(0) {
                tracing::debug!(to = %self.target, "connecting");
            } else {
                tracing::debug!(reconnect_in = ?backoff, "reconnecting");
                tokio::time::sleep(backoff).await;
            }
            let try_connect = async {
                let mut client = TasksClient::connect(self.target.clone()).await?;
                let request = tonic::Request::new(TasksRequest {});
                let stream = client.watch_tasks(request).await?.into_inner();
                Ok::<State, Box<dyn Error + Send + Sync>>(State::Connected { client, stream })
            };
            self.state = match try_connect.await {
                Ok(connected) => {
                    tracing::debug!("connected successfully!");
                    connected
                }
                Err(error) => {
                    tracing::warn!(%error, "error connecting");
                    let backoff = std::cmp::max(backoff + Self::BACKOFF, MAX_BACKOFF);
                    State::Disconnected(backoff)
                }
            };
        }
    }

    pub async fn next_update(&mut self) -> TaskUpdate {
        loop {
            match self.state {
                State::Connected { ref mut stream, .. } => match Pin::new(stream).next().await {
                    Some(Ok(update)) => return update,
                    Some(Err(status)) => {
                        tracing::warn!(%status, "error from stream");
                        self.state = State::Disconnected(Self::BACKOFF);
                    }
                    None => {
                        tracing::error!("stream closed by server");
                        self.state = State::Disconnected(Self::BACKOFF);
                    }
                },
                State::Disconnected(_) => self.connect().await,
            }
        }
    }

    #[tracing::instrument(skip(self))]
    pub async fn watch_details(
        &mut self,
        task_id: u64,
    ) -> Result<Streaming<TaskDetails>, tonic::Status> {
        loop {
            match self.state {
                State::Connected { ref mut client, .. } => {
                    let request = tonic::Request::new(DetailsRequest {
                        id: Some(task_id.into()),
                    });
                    match client.watch_task_details(request).await {
                        Ok(watch) => return Ok(watch.into_inner()),
                        // If the error is a `h2::Error`, that indicates
                        // something went wrong at the connection level, rather
                        // than the server returning an error code. In that
                        // case, let's try reconnecting...
                        Err(error) if error.source().iter().any(|src| src.is::<h2::Error>()) => {
                            tracing::warn!(
                                id = task_id,
                                error = %error,
                                "error watching task details"
                            );
                            self.state = State::Disconnected(Self::BACKOFF);
                        }
                        // Otherwise, return the error.
                        Err(e) => return Err(e),
                    }
                }
                State::Disconnected(_) => self.connect().await,
            }
        }
    }

    pub fn render(&self, styles: &crate::view::Styles) -> tui::text::Spans {
        use tui::{
            style::{Color, Modifier},
            text::{Span, Spans},
        };
        let state = match self.state {
            State::Connected { .. } => Span::styled(
                "(CONNECTED)",
                styles.fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            State::Disconnected(d) if d == Duration::from_secs(0) => Span::styled(
                "(CONNECTING)",
                styles.fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            State::Disconnected(d) => Span::styled(
                format!("(RECONNECTING IN {:?})", d),
                styles.fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
        };
        Spans::from(vec![
            Span::raw("connection: "),
            Span::raw(self.target.to_string()),
            Span::raw(" "),
            state,
        ])
    }
}
