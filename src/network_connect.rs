use crate::interfaces::{Message, NetworkError, ProtocolVersion, RepoId, RepoMessage};
use crate::repo::RepoHandle;
use futures::{Sink, SinkExt, Stream, StreamExt};

/// Which direction a connection passed to [`crate::RepoHandle::new_remote_repo`] is going
pub enum ConnDirection {
    Incoming,
    Outgoing,
}

impl RepoHandle {
    pub async fn connect_stream<Str, Snk, SendErr, RecvErr>(
        &self,
        mut stream: Str,
        mut sink: Snk,
        direction: ConnDirection,
    ) -> Result<(), NetworkError>
    where
        SendErr: std::error::Error + Send + Sync + 'static,
        RecvErr: std::error::Error + Send + Sync + 'static,
        Snk: Sink<Message, Error = SendErr> + Send + 'static + Unpin,
        Str: Stream<Item = Result<Message, RecvErr>> + Send + 'static + Unpin,
    {
        let other_id = self.handshake(&mut stream, &mut sink, direction).await?;
        tracing::trace!(?other_id, repo_id=?self.get_repo_id(), "Handshake complete");

        let stream = stream.map({
            let repo_id = self.get_repo_id().clone();
            move |msg| match msg {
                Ok(Message::Repo(repo_msg)) => {
                    tracing::trace!(?repo_msg, repo_id=?repo_id, "Received repo message");
                    Ok(repo_msg)
                }
                Ok(m) => {
                    tracing::warn!(?m, repo_id=?repo_id, "Received non-repo message");
                    Err(NetworkError::Error(
                        "unexpected non-repo message".to_string(),
                    ))
                }
                Err(e) => {
                    tracing::error!(?e, repo_id=?repo_id, "Error receiving repo message");
                    Err(NetworkError::Error(format!(
                        "error receiving repo message: {}",
                        e
                    )))
                }
            }
        });

        let sink_repo_id = self.get_repo_id().clone();
        let sink = sink
            .with_flat_map::<RepoMessage, _, _>(move |msg| {
                tracing::trace!(?msg, repo_id=?sink_repo_id, "Sending repo message");
                match msg {
                    RepoMessage::Sync { .. } => futures::stream::iter(vec![Ok(Message::Repo(msg))]),
                    _ => futures::stream::iter(vec![]),
                }
            })
            .sink_map_err(|e| {
                tracing::error!(?e, "Error sending repo message");
                NetworkError::Error(format!("error sending repo message: {}", e))
            });

        self.new_remote_repo(other_id, Box::new(stream), Box::new(sink));

        Ok(())
    }

    async fn handshake<Str, Snk, SendErr, RecvErr>(
        &self,
        stream: &mut Str,
        sink: &mut Snk,
        direction: ConnDirection,
    ) -> Result<RepoId, NetworkError>
    where
        SendErr: std::error::Error + Send + Sync + 'static,
        RecvErr: std::error::Error + Send + Sync + 'static,
        Str: Stream<Item = Result<Message, RecvErr>> + Unpin,
        Snk: Sink<Message, Error = SendErr> + Unpin,
    {
        match direction {
            ConnDirection::Incoming => {
                if let Some(msg) = stream.next().await {
                    let other_id = match msg {
                        Ok(Message::Join {
                            sender: other_id, ..
                        }) => other_id,
                        Ok(other) => {
                            return Err(NetworkError::Error(format!(
                                "unexpected message (expecting join): {:?}",
                                other
                            )))
                        }
                        Err(e) => {
                            return Err(NetworkError::Error(format!("error reciving: {}", e)))
                        }
                    };
                    let msg = Message::Peer {
                        sender: self.get_repo_id().clone(),
                        selected_protocol_version: ProtocolVersion::V1,
                    };
                    sink.send(msg)
                        .await
                        .map_err(|e| NetworkError::Error(format!("error sending: {}", e)))?;
                    Ok(other_id)
                } else {
                    Err(NetworkError::Error(
                        "unexpected end of receive stream".to_string(),
                    ))
                }
            }
            ConnDirection::Outgoing => {
                let msg = Message::Join {
                    sender: self.get_repo_id().clone(),
                    supported_protocol_versions: vec![ProtocolVersion::V1],
                };
                sink.send(msg)
                    .await
                    .map_err(|e| NetworkError::Error(format!("send error: {}", e)))?;
                let msg = stream.next().await;
                match msg {
                    Some(Ok(Message::Peer { sender, .. })) => Ok(sender),
                    Some(Ok(other)) => Err(NetworkError::Error(format!(
                        "unexpected message (expecting peer): {:?}",
                        other
                    ))),
                    Some(Err(e)) => Err(NetworkError::Error(format!("error sending: {}", e))),
                    None => Err(NetworkError::Error(
                        "unexpected end of receive stream".to_string(),
                    )),
                }
            }
        }
    }
}
