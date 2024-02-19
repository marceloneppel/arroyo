use std::{
    any::Any,
    collections::{BTreeMap, HashMap},
    pin::Pin,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime},
};

use anyhow::Result;
use arrow::compute::{max, min, partition, sort_to_indices, take};
use arrow_array::{RecordBatch, TimestampNanosecondArray};
use arroyo_df::physical::{ArroyoPhysicalExtensionCodec, DecodingContext};
use arroyo_operator::context::ArrowContext;
use arroyo_operator::operator::{ArrowOperator, OperatorConstructor, OperatorNode, Registry};
use arroyo_rpc::{
    df::{ArroyoSchema, ArroyoSchemaRef},
    grpc::{api, TableConfig},
};
use arroyo_state::timestamp_table_config;
use arroyo_types::{from_nanos, print_time, CheckpointBarrier, Watermark};
use datafusion::execution::context::SessionContext;
use datafusion_execution::{
    runtime_env::{RuntimeConfig, RuntimeEnv},
    SendableRecordBatchStream,
};
use datafusion_proto::{physical_plan::AsExecutionPlan, protobuf::PhysicalPlanNode};
use futures::StreamExt;
use futures::{lock::Mutex, stream::FuturesUnordered, Future};
use prost::Message;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::info;

use super::sync::streams::KeyedCloneableStreamFuture;
type NextBatchFuture<K> = KeyedCloneableStreamFuture<K, SendableRecordBatchStream>;

pub struct InstantJoin {
    left_input_schema: ArroyoSchemaRef,
    right_input_schema: ArroyoSchemaRef,
    execs: BTreeMap<SystemTime, InstantComputeHolder>,
    registry: Arc<Registry>,
    futures: Arc<Mutex<FuturesUnordered<NextBatchFuture<SystemTime>>>>,
    join_physical_plan: PhysicalPlanNode,
}

struct InstantComputeHolder {
    active_exec: NextBatchFuture<SystemTime>,
    left_sender: UnboundedSender<RecordBatch>,
    right_sender: UnboundedSender<RecordBatch>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Side {
    Left,
    Right,
}

impl Side {
    fn name(&self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
        }
    }
}

impl InstantComputeHolder {
    fn insert(&mut self, batch: RecordBatch, side: Side) -> Result<()> {
        match side {
            Side::Left => self.left_sender.send(batch)?,
            Side::Right => self.right_sender.send(batch)?,
        }
        Ok(())
    }
}

impl InstantJoin {
    fn input_schema(&mut self, side: Side) -> ArroyoSchemaRef {
        match side {
            Side::Left => self.left_input_schema.clone(),
            Side::Right => self.right_input_schema.clone(),
        }
    }
    async fn get_or_insert_exec(&mut self, time: SystemTime) -> Result<&mut InstantComputeHolder> {
        if !self.execs.contains_key(&time) {
            let (left_sender, left_receiver) = unbounded_channel();
            let (right_sender, right_receiver) = unbounded_channel();
            // regenerate the execution plan with the new time
            let codec = ArroyoPhysicalExtensionCodec {
                context: DecodingContext::LockedJoinStream {
                    left: Arc::new(RwLock::new(Some(left_receiver))),
                    right: Arc::new(RwLock::new(Some(right_receiver))),
                },
            };
            let join_execution_plan = self.join_physical_plan.try_into_physical_plan(
                self.registry.as_ref(),
                &RuntimeEnv::new(RuntimeConfig::new())?,
                &codec,
            )?;

            let new_exec = join_execution_plan
                .execute(0, SessionContext::new().task_ctx())
                .expect("successfully computed?");
            let next_batch_future = NextBatchFuture::new(time, new_exec);
            self.futures.lock().await.push(next_batch_future.clone());
            let exec = InstantComputeHolder {
                active_exec: next_batch_future,
                left_sender,
                right_sender,
            };
            self.execs.insert(time, exec);
        }
        Ok(self.execs.get_mut(&time).unwrap())
    }

    async fn process_side(
        &mut self,
        side: Side,
        batch: RecordBatch,
        ctx: &mut ArrowContext,
    ) -> Result<()> {
        let table = ctx
            .table_manager
            .get_expiring_time_key_table(side.name(), ctx.last_present_watermark())
            .await
            .expect("should have table");

        let time_column = batch
            .column(self.input_schema(side).timestamp_index)
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .expect("should have timestamp column");
        let max_timestamp = max(time_column).expect("should have max timestamp");
        table.insert(from_nanos(max_timestamp as u128), batch.clone());
        let min_timestamp = min(time_column).expect("should have min timestamp");
        if ctx
            .last_present_watermark()
            .map(|watermark| watermark > from_nanos(min_timestamp as u128))
            .unwrap_or(false)
        {
            panic!(
                "shouldn't have a batch with timestamp {} before the watermark {:?}",
                min_timestamp,
                ctx.last_present_watermark().map(print_time)
            );
        }
        let batch = self.input_schema(side).unkeyed_batch(&batch)?;
        // We expect that a record batch will usually only be a single timestamp, so we special case that.
        if max_timestamp == min_timestamp {
            let exec = self
                .get_or_insert_exec(from_nanos(max_timestamp as u128))
                .await?;
            exec.insert(batch, side)?;
            return Ok(());
        }
        // otherwise, partition by time and send to the appropriate exec
        let indices = sort_to_indices(time_column, None, None).expect("should be able to sort");
        let columns = batch
            .columns()
            .iter()
            .map(|c| take(c, &indices, None).unwrap())
            .collect();
        let sorted = RecordBatch::try_new(batch.schema(), columns).unwrap();
        let sorted_timestamps = take(time_column, &indices, None).unwrap();
        let ranges = partition(&vec![sorted_timestamps.clone()])
            .unwrap()
            .ranges();
        let typed_timestamps = sorted_timestamps
            .as_any()
            .downcast_ref::<TimestampNanosecondArray>()
            .expect("should be able to downcast");
        for range in ranges {
            let batch = sorted.slice(range.start, range.end - range.start);
            let time = from_nanos(typed_timestamps.value(range.start) as u128);
            let exec = self.get_or_insert_exec(time).await?;
            exec.insert(batch, side)?;
        }
        Ok(())
    }
    async fn process_left(
        &mut self,
        record_batch: RecordBatch,
        ctx: &mut ArrowContext,
    ) -> Result<()> {
        self.process_side(Side::Left, record_batch, ctx).await
    }

    async fn process_right(
        &mut self,
        right_batch: RecordBatch,
        ctx: &mut ArrowContext,
    ) -> Result<()> {
        self.process_side(Side::Right, right_batch, ctx).await
    }
}

type PolledFutureT = <NextBatchFuture<SystemTime> as Future>::Output;

#[async_trait::async_trait]
impl ArrowOperator for InstantJoin {
    fn name(&self) -> String {
        format!("InstantJoin")
    }

    async fn on_start(&mut self, ctx: &mut ArrowContext) {
        let watermark = ctx.last_present_watermark();
        let left_table = ctx
            .table_manager
            .get_expiring_time_key_table("left", watermark)
            .await
            .expect("should have left table");
        let left_batches: Vec<_> = left_table
            .all_batches_for_watermark(watermark)
            .flat_map(|(_time, batches)| batches.clone())
            .collect();
        for batch in left_batches {
            self.process_left(batch.clone(), ctx)
                .await
                .expect("should be able to add left from state");
        }
        let right_table = ctx
            .table_manager
            .get_expiring_time_key_table("right", watermark)
            .await
            .expect("should have right table");
        let right_batches: Vec<_> = right_table
            .all_batches_for_watermark(watermark)
            .flat_map(|(_time, batches)| batches.clone())
            .collect();
        for batch in right_batches {
            self.process_right(batch.clone(), ctx)
                .await
                .expect("should be able to add right from state");
        }
    }

    async fn process_batch(&mut self, _record_batch: RecordBatch, _ctx: &mut ArrowContext) {
        unreachable!();
    }
    async fn process_batch_index(
        &mut self,
        index: usize,
        total_inputs: usize,
        record_batch: RecordBatch,
        ctx: &mut ArrowContext,
    ) {
        match index / (total_inputs / 2) {
            0 => self
                .process_left(record_batch, ctx)
                .await
                .expect("should process left"),
            1 => self
                .process_right(record_batch, ctx)
                .await
                .expect("should process right"),
            _ => unreachable!(),
        }
    }
    async fn handle_watermark(
        &mut self,
        int_watermark: Watermark,
        ctx: &mut ArrowContext,
    ) -> Option<Watermark> {
        let Some(watermark) = ctx.last_present_watermark() else {
            return Some(int_watermark);
        };
        let futures_to_drain = {
            let mut futures_to_drain = vec![];
            while !self.execs.is_empty() {
                let first_watermark = self.execs.first_key_value().unwrap().0;
                if *first_watermark >= watermark {
                    break;
                }
                let (_time, exec) = self.execs.pop_first().expect("should have exec");
                futures_to_drain.push(exec.active_exec);
            }
            futures_to_drain
        };
        for mut future in futures_to_drain {
            while let (_time, Some((batch, new_exec))) = future.await {
                match batch {
                    Ok(batch) => {
                        ctx.collect(batch).await;
                    }
                    Err(err) => {
                        info!(
                            "failed with {:?}
                        left input schema:{:?}, 
                        right input schema:{:?}, 
                        plan:{:?}",
                            err,
                            self.left_input_schema,
                            self.right_input_schema,
                            self.join_physical_plan
                        );
                        panic!("error in future: {:?}", err);
                    }
                }
                future = new_exec;
            }
        }
        Some(int_watermark)
    }

    async fn handle_checkpoint(&mut self, _b: CheckpointBarrier, ctx: &mut ArrowContext) {
        let watermark = ctx.last_present_watermark();
        ctx.table_manager
            .get_expiring_time_key_table("left", watermark)
            .await
            .expect("should have left table")
            .flush(watermark)
            .await
            .expect("should flush");
        ctx.table_manager
            .get_expiring_time_key_table("right", watermark)
            .await
            .expect("should have right table")
            .flush(watermark)
            .await
            .expect("should flush");
    }

    fn tables(&self) -> HashMap<String, TableConfig> {
        let mut tables = HashMap::new();
        tables.insert(
            "left".to_string(),
            timestamp_table_config(
                "left",
                "left join data",
                Duration::ZERO,
                self.left_input_schema.as_ref().clone(),
            ),
        );
        tables.insert(
            "right".to_string(),
            timestamp_table_config(
                "right",
                "right join data",
                Duration::ZERO,
                self.right_input_schema.as_ref().clone(),
            ),
        );
        tables
    }

    fn future_to_poll(
        &mut self,
    ) -> Option<Pin<Box<dyn Future<Output = Box<dyn Any + Send>> + Send>>> {
        if self.futures.try_lock().unwrap().is_empty() {
            return None;
        }
        let future = self.futures.clone();
        Some(Box::pin(async move {
            let result: Option<PolledFutureT> = future.lock().await.next().await;
            Box::new(result) as Box<dyn Any + Send>
        }))
    }

    async fn handle_future_result(&mut self, result: Box<dyn Any + Send>, ctx: &mut ArrowContext) {
        let data: Box<Option<PolledFutureT>> = result.downcast().expect("invalid data in future");
        match *data {
            Some((bin, batch_option)) => match batch_option {
                None => {
                    info!("future for {} was finished elsewhere", print_time(bin));
                }
                Some((batch, future)) => match self.execs.get_mut(&bin) {
                    Some(exec) => {
                        exec.active_exec = future.clone();
                        ctx.collect(batch.expect("should compute batch in future"))
                            .await;
                        self.futures.lock().await.push(future);
                    }
                    None => unreachable!(
                        "FuturesUnordered returned a batch, but we can't find the exec"
                    ),
                },
            },
            None => {}
        }
    }
}

pub struct InstantJoinConstructor;
impl OperatorConstructor for InstantJoinConstructor {
    type ConfigT = api::JoinOperator;
    fn with_config(
        &self,
        config: Self::ConfigT,
        registry: Arc<Registry>,
    ) -> anyhow::Result<OperatorNode> {
        let join_physical_plan_node = PhysicalPlanNode::decode(&mut config.join_plan.as_slice())?;

        let left_input_schema: Arc<ArroyoSchema> =
            Arc::new(config.left_schema.unwrap().try_into()?);
        let right_input_schema: Arc<ArroyoSchema> =
            Arc::new(config.right_schema.unwrap().try_into()?);

        Ok(OperatorNode::from_operator(Box::new(InstantJoin {
            left_input_schema,
            right_input_schema,
            execs: BTreeMap::new(),
            registry,
            futures: Arc::new(Mutex::new(FuturesUnordered::new())),
            join_physical_plan: join_physical_plan_node,
        })))
    }
}