use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use itertools::Itertools;
use segment::entry::entry_point::SegmentEntry;
use segment::types::{
    ExtendedPointId, Filter, PayloadIndexInfo, PayloadKeyType, ScoredPoint, SegmentType,
    WithPayload, WithPayloadInterface, WithVector,
};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::collection_manager::holders::segment_holder::LockedSegment;
use crate::collection_manager::segments_searcher::SegmentsSearcher;
use crate::operations::types::{
    CollectionInfo, CollectionResult, CollectionStatus, CountRequest, CountResult,
    OptimizersStatus, PointRequest, Record, SearchRequestBatch, UpdateResult, UpdateStatus,
};
use crate::operations::CollectionUpdateOperations;
use crate::shard::local_shard::LocalShard;
use crate::shard::ShardOperation;
use crate::update_handler::{OperationData, UpdateSignal};

#[async_trait]
impl ShardOperation for LocalShard {
    /// Imply interior mutability.
    /// Performs update operation on this collection asynchronously.
    /// Explicitly waits for result to be updated.
    async fn update(
        &self,
        operation: CollectionUpdateOperations,
        wait: bool,
    ) -> CollectionResult<UpdateResult> {
        let (callback_sender, callback_receiver) = if wait {
            let (tx, rx) = oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        let operation_id = {
            let update_sender = self.update_sender.load();
            let channel_permit = update_sender.reserve().await?;
            let mut wal_lock = self.wal.lock();
            let operation_id = wal_lock.write(&operation)?;
            channel_permit.send(UpdateSignal::Operation(OperationData {
                op_num: operation_id,
                operation,
                sender: callback_sender,
            }));
            operation_id
        };

        if let Some(receiver) = callback_receiver {
            let _res = receiver.await??;
            Ok(UpdateResult {
                operation_id,
                status: UpdateStatus::Completed,
            })
        } else {
            Ok(UpdateResult {
                operation_id,
                status: UpdateStatus::Acknowledged,
            })
        }
    }

    async fn scroll_by(
        &self,
        offset: Option<ExtendedPointId>,
        limit: usize,
        with_payload_interface: &WithPayloadInterface,
        with_vector: &WithVector,
        filter: Option<&Filter>,
    ) -> CollectionResult<Vec<Record>> {
        // ToDo: Make faster points selection with a set
        let segments = self.segments();
        let point_ids = segments
            .read()
            .iter()
            .flat_map(|(_, segment)| {
                segment
                    .get()
                    .read()
                    .read_filtered(offset, Some(limit), filter)
            })
            .sorted()
            .dedup()
            .take(limit)
            .collect_vec();

        let with_payload = WithPayload::from(with_payload_interface);
        let mut points =
            SegmentsSearcher::retrieve(segments, &point_ids, &with_payload, with_vector).await?;
        points.sort_by_key(|point| point.id);

        Ok(points)
    }

    /// Collect overview information about the shard
    async fn info(&self) -> CollectionResult<CollectionInfo> {
        let collection_config = self.config.read().await.clone();
        let segments = self.segments().read();
        let mut vectors_count = 0;
        let mut indexed_vectors_count = 0;
        let mut points_count = 0;
        let mut segments_count = 0;
        let mut status = CollectionStatus::Green;
        let mut schema: HashMap<PayloadKeyType, PayloadIndexInfo> = Default::default();
        for (_idx, segment) in segments.iter() {
            segments_count += 1;

            let segment_info = match segment {
                LockedSegment::Original(original_segment) => {
                    let info = original_segment.read().info();
                    if info.segment_type == SegmentType::Indexed {
                        indexed_vectors_count += info.num_vectors;
                    }
                    info
                }
                LockedSegment::Proxy(proxy_segment) => {
                    let proxy_segment_lock = proxy_segment.read();
                    let proxy_segment_info = proxy_segment_lock.info();

                    let wrapped_info = proxy_segment_lock.wrapped_segment.get().read().info();
                    if wrapped_info.segment_type == SegmentType::Indexed {
                        indexed_vectors_count += wrapped_info.num_vectors;
                    }
                    proxy_segment_info
                }
            };

            if segment_info.segment_type == SegmentType::Special {
                status = CollectionStatus::Yellow;
            }
            vectors_count += segment_info.num_vectors;
            points_count += segment_info.num_points;
            for (key, val) in segment_info.index_schema {
                schema.insert(key, val);
            }
        }
        if !segments.failed_operation.is_empty() || segments.optimizer_errors.is_some() {
            status = CollectionStatus::Red;
        }

        let optimizer_status = match &segments.optimizer_errors {
            None => OptimizersStatus::Ok,
            Some(error) => OptimizersStatus::Error(error.to_string()),
        };

        Ok(CollectionInfo {
            status,
            optimizer_status,
            vectors_count,
            indexed_vectors_count,
            points_count,
            segments_count,
            config: collection_config,
            payload_schema: schema,
        })
    }

    async fn search(
        &self,
        request: Arc<SearchRequestBatch>,
        search_runtime_handle: &Handle,
    ) -> CollectionResult<Vec<Vec<ScoredPoint>>> {
        let collection_params = self.config.read().await.params.clone();
        // check vector names existing
        for req in &request.searches {
            collection_params.get_vector_params(req.vector.get_name())?;
        }
        let res = SegmentsSearcher::search(self.segments(), request.clone(), search_runtime_handle)
            .await?;
        let top_results = res
            .into_iter()
            .zip(request.searches.iter())
            .map(|(vector_res, req)| {
                let vector_name = req.vector.get_name();
                let distance = collection_params
                    .get_vector_params(vector_name)
                    .unwrap()
                    .distance;
                let processed_res = vector_res.into_iter().map(|mut scored_point| {
                    scored_point.score = distance.postprocess_score(scored_point.score);
                    scored_point
                });

                if let Some(threshold) = req.score_threshold {
                    processed_res
                        .take_while(|scored_point| {
                            distance.check_threshold(scored_point.score, threshold)
                        })
                        .collect()
                } else {
                    processed_res.collect()
                }
            })
            .collect();
        Ok(top_results)
    }

    async fn count(&self, request: Arc<CountRequest>) -> CollectionResult<CountResult> {
        let total_count = if request.exact {
            let all_points = self.read_filtered(request.filter.as_ref()).await?;
            all_points.len()
        } else {
            self.estimate_cardinality(request.filter.as_ref())
                .await?
                .exp
        };
        Ok(CountResult { count: total_count })
    }

    async fn retrieve(
        &self,
        request: Arc<PointRequest>,
        with_payload: &WithPayload,
        with_vector: &WithVector,
    ) -> CollectionResult<Vec<Record>> {
        SegmentsSearcher::retrieve(self.segments(), &request.ids, with_payload, with_vector).await
    }
}
