use automerge::transaction::Transactable;
use automerge::ReadDoc;
use automerge_repo::{
    NetworkAdapter, NetworkError, NetworkEvent, NetworkMessage, Repo, RepoId, StorageAdapter,
};
use core::pin::Pin;
use futures::sink::Sink;
use futures::stream::Stream;
use futures::task::{Context, Poll, Waker};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Sender};

#[derive(Debug, Clone)]
struct Network<NetworkMessage> {
    buffer: Arc<Mutex<VecDeque<NetworkEvent>>>,
    stream_waker: Arc<Mutex<Option<Waker>>>,
    outgoing: Arc<Mutex<VecDeque<NetworkMessage>>>,
    sink_waker: Arc<Mutex<Option<Waker>>>,
    sender: Sender<(RepoId, RepoId)>,
}

impl Network<NetworkMessage> {
    pub fn new(sender: Sender<(RepoId, RepoId)>) -> Self {
        let buffer = Arc::new(Mutex::new(VecDeque::new()));
        let stream_waker = Arc::new(Mutex::new(None));
        let sink_waker = Arc::new(Mutex::new(None));
        let outgoing = Arc::new(Mutex::new(VecDeque::new()));
        Network {
            buffer: buffer.clone(),
            stream_waker: stream_waker.clone(),
            outgoing: outgoing.clone(),
            sender,
            sink_waker: sink_waker.clone(),
        }
    }

    pub fn receive_incoming(&self, event: NetworkEvent) {
        self.buffer.lock().push_back(event);
        if let Some(waker) = self.stream_waker.lock().take() {
            waker.wake();
        }
    }

    pub fn take_outgoing(&self) -> NetworkMessage {
        let message = self.outgoing.lock().pop_front().unwrap();
        if let Some(waker) = self.sink_waker.lock().take() {
            waker.wake();
        }
        message
    }
}

impl Stream for Network<NetworkMessage> {
    type Item = NetworkEvent;
    fn poll_next(
        self: Pin<&mut Network<NetworkMessage>>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<NetworkEvent>> {
        *self.stream_waker.lock() = Some(cx.waker().clone());
        if let Some(event) = self.buffer.lock().pop_front() {
            Poll::Ready(Some(event))
        } else {
            Poll::Pending
        }
    }
}

impl Sink<NetworkMessage> for Network<NetworkMessage> {
    type Error = NetworkError;
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        *self.sink_waker.lock() = Some(cx.waker().clone());
        if self.outgoing.lock().is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
    fn start_send(self: Pin<&mut Self>, item: NetworkMessage) -> Result<(), Self::Error> {
        let (from_repo_id, to_repo_id) = match &item {
            NetworkMessage::Sync {
                from_repo_id,
                to_repo_id,
                ..
            } => (from_repo_id.clone(), to_repo_id.clone()),
        };

        self.outgoing.lock().push_back(item);
        if self
            .sender
            .blocking_send((from_repo_id, to_repo_id))
            .is_err()
        {
            return Err(NetworkError::Error);
        }
        Ok(())
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        *self.sink_waker.lock() = Some(cx.waker().clone());
        if self.outgoing.lock().is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        *self.sink_waker.lock() = Some(cx.waker().clone());
        if self.outgoing.lock().is_empty() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

impl NetworkAdapter for Network<NetworkMessage> {}

struct Storage;

impl StorageAdapter for Storage {}

#[test]
fn test_repo_stop() {
    // Create the repo.
    let repo = Repo::new(None, None, Box::new(Storage));

    // Run the repo in the background.
    let repo_handle = repo.run();

    // Stop the repo.
    repo_handle.stop().unwrap();
}

#[test]
fn test_simple_sync() {
    let (sender, mut network_receiver) = channel(1);
    let mut repo_handles = vec![];
    let mut documents = vec![];
    let mut peers = HashMap::new();
    let synced = Arc::new(Mutex::new(0));
    let (done_sync_sender, mut done_sync_receiver) = channel(1);

    for _ in 1..10 {
        // Create the repo.
        let synced_clone = synced.clone();
        let done_sync_sender = done_sync_sender.clone();
        let repo = Repo::new(Some(Box::new(move |_synced| {})), None, Box::new(Storage));
        let mut repo_handle = repo.run();

        // Create a document.
        let mut doc_handle = repo_handle.new_document();
        doc_handle.with_doc_mut(|doc| {
            doc.put(
                automerge::ROOT,
                "repo_id",
                format!("{}", repo_handle.get_repo_id()),
            )
            .expect("Failed to change the document.");
            doc.commit();
        });
        documents.push(doc_handle);

        repo_handles.push(repo_handle);
    }

    let repo_handles_clone = repo_handles.clone();

    let repo_ids: Vec<RepoId> = repo_handles
        .iter()
        .map(|handle| handle.get_repo_id().clone())
        .collect();
    for repo_handle in repo_handles.iter() {
        for id in repo_ids.iter() {
            // Create the network adapter.
            let network = Network::new(sender.clone());
            repo_handle.new_network_adapter(id.clone(), Box::new(network.clone()));
            let entry = peers
                .entry(repo_handle.get_repo_id().clone())
                .or_insert(HashMap::new());
            entry.insert(id.clone(), network);
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.spawn(async move {
        // A router of network messages.
        loop {
            tokio::select! {
               msg = network_receiver.recv() => {
                   let (from_repo_id, to_repo_id) = msg.unwrap();
                   let incoming = {
                       let peers = peers.get_mut(&from_repo_id).unwrap();
                       let peer = peers.get_mut(&to_repo_id).unwrap();
                       peer.take_outgoing()
                   };
                   match incoming {
                       NetworkMessage::Sync {
                           from_repo_id,
                           to_repo_id,
                           document_id,
                           message,
                       } => {
                           let peers = peers.get_mut(&to_repo_id).unwrap();
                           let peer = peers.get_mut(&from_repo_id).unwrap();
                           peer.receive_incoming(NetworkEvent::Sync {
                               from_repo_id,
                               to_repo_id,
                               document_id,
                               message,
                           });
                       }
                   }
               },
            }
        }
    });

    rt.spawn(async move {
        let mut synced = 0;
        for doc_handle in documents {
            for repo_handle in repo_handles_clone.iter() {
                if doc_handle.document_id().get_repo_id() == repo_handle.get_repo_id() {
                    continue;
                }
                repo_handle.request_document(doc_handle.document_id()).await;
                synced = synced + 1;
            }
        }
        assert_eq!(synced, 72);
        let _ = done_sync_sender.try_send(());
    });

    done_sync_receiver.blocking_recv().unwrap();

    // Stop repo.
    for handle in repo_handles.into_iter() {
        handle.stop().unwrap();
    }
}

#[test]
fn test_requesting_document_connected_peers() {
    // Create two repos.
    let repo_1 = Repo::new(None, None, Box::new(Storage));
    let repo_2 = Repo::new(None, None, Box::new(Storage));

    // Run the repos in the background.
    let mut repo_handle_1 = repo_1.run();
    let repo_handle_2 = repo_2.run();

    // Create a document for one repo.
    let mut document_handle_1 = repo_handle_1.new_document();

    // Edit the document.
    document_handle_1.with_doc_mut(|doc| {
        doc.put(
            automerge::ROOT,
            "repo_id",
            format!("{}", repo_handle_1.get_repo_id()),
        )
        .expect("Failed to change the document.");
        doc.commit();
    });

    // Add network adapters
    let mut peers = HashMap::new();
    let (sender, mut network_receiver) = channel(1);
    let network_1 = Network::new(sender.clone());
    let network_2 = Network::new(sender.clone());
    repo_handle_1.new_network_adapter(
        repo_handle_2.get_repo_id().clone(),
        Box::new(network_1.clone()),
    );
    repo_handle_2.new_network_adapter(
        repo_handle_1.get_repo_id().clone(),
        Box::new(network_2.clone()),
    );
    peers.insert(repo_handle_2.get_repo_id().clone(), network_1);
    peers.insert(repo_handle_1.get_repo_id().clone(), network_2);

    // Request the document.
    let repo_handle_future = repo_handle_2.request_document(document_handle_1.document_id());

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // Spawn a task that awaits the requested doc handle.
    let (done_sync_sender, mut done_sync_receiver) = channel(1);
    rt.spawn(async move {
        let _doc_handle = repo_handle_future.await;
        done_sync_sender.send(()).await.unwrap();
    });

    rt.spawn(async move {
        // A router of network messages.
        loop {
            tokio::select! {
               msg = network_receiver.recv() => {
                   let (_from_repo_id, to_repo_id) = msg.unwrap();
                   let incoming = {
                       let peer = peers.get_mut(&to_repo_id).unwrap();
                       peer.take_outgoing()
                   };
                   match incoming {
                       NetworkMessage::Sync {
                           from_repo_id,
                           to_repo_id,
                           document_id,
                           message,
                       } => {
                           let peer = peers.get_mut(&from_repo_id).unwrap();
                           peer.receive_incoming(NetworkEvent::Sync {
                               from_repo_id,
                               to_repo_id,
                               document_id,
                               message,
                           });
                       }
                   }
               },
            }
        }
    });

    done_sync_receiver.blocking_recv().unwrap();

    // Stop the repos.
    repo_handle_1.stop().unwrap();
    repo_handle_2.stop().unwrap();
}

#[test]
fn test_requesting_document_unconnected_peers() {
    // Create two repos.
    let repo_1 = Repo::new(None, None, Box::new(Storage));
    let repo_2 = Repo::new(None, None, Box::new(Storage));

    // Run the repos in the background.
    let mut repo_handle_1 = repo_1.run();
    let repo_handle_2 = repo_2.run();

    // Create a document for one repo.
    let mut document_handle_1 = repo_handle_1.new_document();

    // Edit the document.
    document_handle_1.with_doc_mut(|doc| {
        doc.put(
            automerge::ROOT,
            "repo_id",
            format!("{}", repo_handle_1.get_repo_id()),
        )
        .expect("Failed to change the document.");
        doc.commit();
    });

    // Note: requesting the document while peers aren't connected yet.

    // Request the document.
    let repo_handle_future = repo_handle_2.request_document(document_handle_1.document_id());

    // Add network adapters
    let mut peers = HashMap::new();
    let (sender, mut network_receiver) = channel(1);
    let network_1 = Network::new(sender.clone());
    let network_2 = Network::new(sender.clone());
    repo_handle_1.new_network_adapter(
        repo_handle_2.get_repo_id().clone(),
        Box::new(network_1.clone()),
    );
    repo_handle_2.new_network_adapter(
        repo_handle_1.get_repo_id().clone(),
        Box::new(network_2.clone()),
    );
    peers.insert(repo_handle_2.get_repo_id().clone(), network_1);
    peers.insert(repo_handle_1.get_repo_id().clone(), network_2);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    // Spawn a task that awaits the requested doc handle.
    let (done_sync_sender, mut done_sync_receiver) = channel(1);
    rt.spawn(async move {
        let _doc_handle = repo_handle_future.await;
        done_sync_sender.send(()).await.unwrap();
    });

    rt.spawn(async move {
        // A router of network messages.
        loop {
            tokio::select! {
               msg = network_receiver.recv() => {
                   let (_from_repo_id, to_repo_id) = msg.unwrap();
                   let incoming = {
                       let peer = peers.get_mut(&to_repo_id).unwrap();
                       peer.take_outgoing()
                   };
                   match incoming {
                       NetworkMessage::Sync {
                           from_repo_id,
                           to_repo_id,
                           document_id,
                           message,
                       } => {
                           let peer = peers.get_mut(&from_repo_id).unwrap();
                           peer.receive_incoming(NetworkEvent::Sync {
                               from_repo_id,
                               to_repo_id,
                               document_id,
                               message,
                           });
                       }
                   }
               },
            }
        }
    });

    done_sync_receiver.blocking_recv().unwrap();

    // Stop the repos.
    repo_handle_1.stop().unwrap();
    repo_handle_2.stop().unwrap();
}
