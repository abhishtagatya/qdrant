use actix_web::rt::time::Instant;
use actix_web::{delete, get, web, Responder};
use storage::content_manager::consensus_ops::ConsensusOperations;
use storage::content_manager::errors::StorageError;
use storage::dispatcher::Dispatcher;

use crate::actix::helpers::process_response;

#[get("/cluster")]
async fn cluster_status(dispatcher: web::Data<Dispatcher>) -> impl Responder {
    let timing = Instant::now();
    let response = dispatcher.cluster_status();
    process_response(Ok(response), timing)
}

#[delete("/cluster/peer/{peer_id}")]
async fn remove_peer(dispatcher: web::Data<Dispatcher>, peer_id: web::Path<u64>) -> impl Responder {
    let timing = Instant::now();
    let dispatcher = dispatcher.into_inner();
    let peer_id = peer_id.into_inner();

    if dispatcher.peer_has_shards(peer_id).await {
        return process_response(
            Err(StorageError::BadRequest {
                description: format!("Cannot remove peer {peer_id} as there are shards on it"),
            }),
            timing,
        );
    }

    let response = match dispatcher.consensus_state() {
        Some(consensus_state) => {
            consensus_state
                .propose_consensus_op_with_await(ConsensusOperations::RemovePeer(peer_id), None)
                .await
        }
        None => Err(StorageError::BadRequest {
            description: "Distributed deployment is disabled.".to_string(),
        }),
    };
    process_response(response, timing)
}

// Configure services
pub fn config_cluster_api(cfg: &mut web::ServiceConfig) {
    cfg.service(cluster_status).service(remove_peer);
}
