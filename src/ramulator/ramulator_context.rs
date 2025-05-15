use std::collections::{vec_deque, HashMap, VecDeque};

use dam::channel::PeekResult;
use dam::context_tools::*;
use derive_more::Constructor;
use num::BigUint;

use super::access::{Access, AccessLike, MemoryData, SimpleRead, SimpleWrite};
use super::address::ByteAddress;
use super::request_manager::RequestManager;

/// For HBM2/HBM3:
/// The interface width is 128 bits (16 bytes) per channel
/// The burst length is 2, meaning each request transfers 2 bursts
/// Each request therefore transfers 16 bytes × 2 = 32 bytes
pub static ADDR_OFFSET: u64 = 32;

// Configuration for batch processing
const BATCH_SIZE: usize = 512; // Maximum batch size
const BATCH_INTERVAL: u64 = 1; // Process batches every N cycles
const MAX_BATCHES_PER_CYCLE: usize = 256; // Process at most N batches per cycle

type Recv<'a, T> = dyn dam::channel::adapters::RecvAdapter<T> + Sync + Send + 'a;
type Snd<'a, T> = dyn dam::channel::adapters::SendAdapter<T> + Sync + Send + 'a;

// Define a batch structure to group memory requests
#[derive(Debug)]
struct RequestBatch {
    reads: Vec<Access>,
    writes: Vec<Access>,
    priority: u8, // Higher priority batches get processed first
}

impl RequestBatch {
    fn new() -> Self {
        Self {
            reads: Vec::with_capacity(BATCH_SIZE / 2),
            writes: Vec::with_capacity(BATCH_SIZE / 2),
            priority: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.reads.is_empty() && self.writes.is_empty()
    }

    fn size(&self) -> usize {
        self.reads.len() + self.writes.len()
    }

    fn is_full(&self) -> bool {
        self.size() >= BATCH_SIZE
    }

    fn add_request(&mut self, access: Access) -> bool {
        if self.is_full() {
            return false;
        }

        match access {
            Access::SimpleRead(_) => self.reads.push(access),
            Access::SimpleWrite(_) => self.writes.push(access),
        }

        true
    }
}

pub struct Memory {
    storage: HashMap<u64, MemoryData>,
}

#[derive(Constructor)]
pub struct WriteBundle<'a> {
    pub data: Box<Recv<'a, MemoryData>>,
    pub addr: Box<Recv<'a, u64>>,
    pub ack: Box<Snd<'a, bool>>,
}

#[derive(Constructor)]
pub struct ReadBundle<'a> {
    pub addr: Box<Recv<'a, u64>>,
    pub resp: Box<Snd<'a, MemoryData>>,
    pub resp_addr: Box<Snd<'a, u64>>,
}

#[context_macro]
pub struct RamulatorContext<'a> {
    ramulator: ramulator_wrapper::RamulatorWrapper,
    datastore: Option<Memory>,
    writers: Vec<WriteBundle<'a>>,
    readers: Vec<ReadBundle<'a>>,
    request_backlog: VecDeque<Access>,

    // Batch processing structures
    request_batches: VecDeque<RequestBatch>,
    current_batch: RequestBatch,
    last_batch_cycle: u64,

    // (frequency of the HBM) : (frequency of the Accelerator)
    // Currently I am using (5u32, 9u32)
    // 1GHz (HBM) & 1.8GHz (Accelerator)
    cycles_per_tick: (num_bigint::BigUint, num_bigint::BigUint),
    elapsed_cycles: num_bigint::BigUint,
}

impl Context for RamulatorContext<'_> {
    fn run(&mut self) {
        let mut request_manager = RequestManager::default();

        while self.continue_running(&request_manager) {
            // Process backlog first
            self.process_backlog(&mut request_manager);

            // Process any available responses
            while self.ramulator.ret_available() {
                let resp_loc = ByteAddress(self.ramulator.pop());
                let read = request_manager.register_recv(resp_loc);

                // Handle read request
                let data = match self.datastore {
                    Some(_) => self.read_data(resp_loc.into()).clone(),
                    None => MemoryData::default(),
                };
                self.readers[read.bundle_index()]
                    .resp
                    .enqueue(
                        &self.time,
                        ChannelElement {
                            time: self.time.tick() + 1,
                            data,
                        },
                    )
                    .unwrap();
                self.readers[read.bundle_index()]
                    .resp_addr
                    .enqueue(
                        &self.time,
                        ChannelElement {
                            time: self.time.tick() + 1,
                            data: resp_loc.0,
                        },
                    )
                    .unwrap();
                break;
            }

            // Update ticks
            self.update_ticks();

            // Collect new requests
            self.collect_read_requests();
            self.collect_write_requests();

            // Process batches at regular intervals or when full
            self.process_batches(&mut request_manager);

            self.time.incr_cycles(1);
            self.update_ticks();
        }
    }
}

impl<'a> RamulatorContext<'a> {
    pub fn new<A, B>(config: &str, cycles_per_tick: (A, B), datastore: Option<Memory>) -> Self
    where
        A: Into<BigUint>,
        B: Into<BigUint>,
    {
        Self {
            ramulator: ramulator_wrapper::RamulatorWrapper::new(config),
            datastore,
            writers: vec![],
            readers: vec![],
            request_backlog: VecDeque::new(),

            // Initialize batch processing structures
            request_batches: VecDeque::new(),
            current_batch: RequestBatch::new(),
            last_batch_cycle: 0,

            cycles_per_tick: (cycles_per_tick.0.into(), cycles_per_tick.1.into()),
            elapsed_cycles: 0u32.into(),
            context_info: Default::default(),
        }
    }

    pub fn add_reader(
        &mut self,
        ReadBundle {
            addr,
            resp,
            resp_addr,
        }: ReadBundle<'a>,
    ) {
        addr.attach_receiver(self);
        resp.attach_sender(self);
        resp_addr.attach_sender(self);
        self.readers.push(ReadBundle {
            addr,
            resp,
            resp_addr,
        });
    }

    pub fn add_writer(&mut self, WriteBundle { data, addr, ack }: WriteBundle<'a>) {
        data.attach_receiver(self);
        addr.attach_receiver(self);
        ack.attach_sender(self);
        self.writers.push(WriteBundle { data, addr, ack })
    }

    // Requires a special handler for converting global "tick" count to local cycle count.
    fn update_ticks(&mut self) {
        let cur_ticks = self.time.tick().time();
        let expected_cycles = (cur_ticks * &self.cycles_per_tick.0) / &self.cycles_per_tick.1;
        while self.elapsed_cycles < expected_cycles {
            self.elapsed_cycles += 1u32;
            self.ramulator.tick();
        }
    }

    // Modified to collect write requests into batches instead of sending immediately
    fn collect_write_requests(&mut self) {
        let cur_time = self.time.tick();

        for (ind, writer) in self.writers.iter().enumerate() {
            match (writer.addr.peek(), writer.data.peek()) {
                (
                    PeekResult::Something(ChannelElement {
                        time: addr_time,
                        data: addr_data,
                    }),
                    PeekResult::Something(ChannelElement {
                        time: data_time,
                        data,
                    }),
                ) => {
                    if addr_time <= cur_time && data_time <= cur_time {
                        // Pop the peeked values
                        writer.addr.dequeue(&self.time).unwrap();
                        writer.data.dequeue(&self.time).unwrap();
                        let base_address = addr_data.try_into().unwrap();
                        let access: Access = SimpleWrite::new(base_address, data, ind).into();

                        // Add to current batch
                        if !self.current_batch.add_request(access.clone()) {
                            // Current batch is full, queue it and create a new one
                            if !self.current_batch.is_empty() {
                                self.request_batches.push_back(std::mem::replace(
                                    &mut self.current_batch,
                                    RequestBatch::new(),
                                ));
                            }

                            // Try adding to the new batch
                            if !self.current_batch.add_request(access.clone()) {
                                // Should never happen with a fresh batch, but just in case
                                self.request_backlog.push_back(access);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Modified to collect read requests into batches
    fn collect_read_requests(&mut self) {
        let cur_time = self.time.tick();

        // Collect up to batch size reads per cycle
        for _ in 0..BATCH_SIZE {
            let mut earliest_time = None;
            let mut earliest_index = None;

            // Select the earliest arriving request
            for (ind, reader) in self.readers.iter().enumerate() {
                if let PeekResult::Something(ChannelElement { time, .. }) = reader.addr.peek() {
                    if time <= cur_time {
                        match earliest_time {
                            Some(t) if time < t => {
                                earliest_time = Some(time);
                                earliest_index = Some(ind);
                            }
                            None => {
                                earliest_time = Some(time);
                                earliest_index = Some(ind);
                            }
                            _ => {}
                        }
                    }
                }
            }

            // If no valid request is found, break the loop
            if earliest_index.is_none() {
                break;
            }

            // Process the reader with the earliest request
            let ind = earliest_index.unwrap();
            let reader = &self.readers[ind];

            if let PeekResult::Something(ChannelElement {
                time: _,
                data: addr,
            }) = reader.addr.peek()
            {
                // Pop the peeked value and create a new access request
                reader.addr.dequeue(&self.time).unwrap();
                let access: Access = SimpleRead::new(ByteAddress(addr), ind).into();

                // Add to current batch
                if !self.current_batch.add_request(access.clone()) {
                    // Current batch is full, queue it and create a new one
                    if !self.current_batch.is_empty() {
                        self.request_batches.push_back(std::mem::replace(
                            &mut self.current_batch,
                            RequestBatch::new(),
                        ));
                    }

                    // Try adding to the new batch
                    if !self.current_batch.add_request(access.clone()) {
                        // Should never happen with a fresh batch, but just in case
                        self.request_backlog.push_back(access);
                    }
                }
            }
        }
    }

    // New method for processing batched requests
    fn process_batches(&mut self, request_manager: &mut RequestManager) {
        let current_cycle = self.time.tick().time();

        // Check if it's time to process batches or if current batch is full
        let should_process =
            current_cycle - self.last_batch_cycle >= BATCH_INTERVAL || self.current_batch.is_full();

        if should_process {
            // Queue current batch if not empty
            if !self.current_batch.is_empty() {
                self.request_batches.push_back(std::mem::replace(
                    &mut self.current_batch,
                    RequestBatch::new(),
                ));
            }

            self.last_batch_cycle = current_cycle;

            // Process batches, limited by MAX_BATCHES_PER_CYCLE
            let batch_count = std::cmp::min(self.request_batches.len(), MAX_BATCHES_PER_CYCLE);
            for _ in 0..batch_count {
                if let Some(batch) = self.request_batches.pop_front() {
                    // Process all writes first (optimization for write combining)
                    for access in batch.writes {
                        self.enqueue_payload(access, request_manager);
                    }

                    // Then process all reads
                    for access in batch.reads {
                        self.enqueue_payload(access, request_manager);
                    }
                }
            }
        }
    }

    // Largely unchanged, sends a single request to Ramulator
    fn enqueue_payload(&mut self, access: Access, manager: &mut RequestManager) {
        let enqueue_success = self
            .ramulator
            .send(access.get_addr().into(), access.is_write());

        if enqueue_success {
            match access {
                Access::SimpleRead(rd) => manager.add_request(rd.into()),
                Access::SimpleWrite(write) => {
                    let index = write.bundle_index();
                    let data = write.payload;

                    match self.datastore {
                        Some(_) => {
                            // Store data in memory for future reads
                            self.write_data(write.get_addr().into(), data);
                        }
                        None => {}
                    }

                    self.writers[index]
                        .ack
                        .enqueue(
                            &self.time,
                            ChannelElement {
                                time: self.time.tick() + 1,
                                data: true,
                            },
                        )
                        .unwrap();
                }
            }
        } else {
            self.request_backlog.push_back(access);
        }
    }

    // Modified to process backlog in batches
    fn process_backlog(&mut self, request_manager: &mut RequestManager) {
        let retry_limit = 10;
        let mut retries = 0;

        // Create a dedicated backlog batch
        let mut backlog_batch = RequestBatch::new();
        backlog_batch.priority = 10; // Higher priority for backlogged requests

        // Fill batch from backlog
        while retries < retry_limit && !self.request_backlog.is_empty() && !backlog_batch.is_full()
        {
            let access = self.request_backlog.pop_front().unwrap();
            if !backlog_batch.add_request(access.clone()) {
                // Should rarely happen, but just in case
                self.request_backlog.push_front(access);
                break;
            }
            retries += 1;
        }

        // Process the backlog batch
        if !backlog_batch.is_empty() {
            // Process writes first
            for access in backlog_batch.writes {
                if self
                    .ramulator
                    .send(access.get_addr().into(), access.is_write())
                {
                    match access {
                        Access::SimpleWrite(write) => {
                            let index = write.bundle_index();
                            let data = write.payload;

                            match self.datastore {
                                Some(_) => {
                                    // Store data in memory for future reads
                                    self.write_data(write.get_addr().into(), data);
                                }
                                None => {}
                            }
                            self.writers[index]
                                .ack
                                .enqueue(
                                    &self.time,
                                    ChannelElement {
                                        time: self.time.tick() + 1,
                                        data: true,
                                    },
                                )
                                .unwrap();
                        }
                        _ => {} // Should never happen in writes list
                    }
                } else {
                    // Failed to send, put back in backlog
                    self.request_backlog.push_back(access);
                }
            }

            // Then process reads
            for access in backlog_batch.reads {
                if self
                    .ramulator
                    .send(access.get_addr().into(), access.is_write())
                {
                    match access {
                        Access::SimpleRead(rd) => request_manager.add_request(rd.into()),
                        _ => {} // Should never happen in reads list
                    }
                } else {
                    // Failed to send, put back in backlog
                    self.request_backlog.push_back(access);
                }
            }
        }
    }

    fn read_data(&self, addr: u64) -> MemoryData {
        match self.datastore {
            Some(ref datastore) => match datastore.read(addr) {
                Some(data) => data.clone(),
                None => panic!("Expected value in memory"),
            },
            None => panic!("Datastore not initialized"),
        }
    }

    fn write_data(&mut self, addr: u64, data: MemoryData) {
        match self.datastore {
            Some(ref mut datastore) => {
                // Store data in memory for future reads
                datastore.storage.insert(addr, data);
            }
            None => panic!("Datastore not initialized"),
        }
    }

    fn continue_running(&mut self, request_manager: &RequestManager) -> bool {
        if !request_manager.is_empty() {
            return true;
        }

        if !self.request_backlog.is_empty() {
            return true;
        }

        if !self.current_batch.is_empty() || !self.request_batches.is_empty() {
            return true;
        }

        // check all of the writers
        let mut writers_done =
            self.writers
                .iter()
                .all(
                    |WriteBundle { data, addr, ack: _ }| match (data.peek(), addr.peek()) {
                        (PeekResult::Closed, _) | (_, PeekResult::Closed) => true,
                        _ => false,
                    },
                );

        if self.writers.is_empty() {
            writers_done = true;
        }

        if !writers_done {
            return true;
        }

        let readers_done = self.readers.iter().all(
            |ReadBundle {
                 addr,
                 resp: _,
                 resp_addr: _,
             }| {
                match addr.peek() {
                    PeekResult::Closed => true,
                    _ => false,
                }
            },
        );

        if !readers_done {
            return true;
        }

        false
    }
}

impl Memory {
    pub fn new() -> Self {
        Memory {
            storage: HashMap::new(),
        }
    }

    pub fn read(&self, address: u64) -> Option<&MemoryData> {
        self.storage.get(&address)
    }
}

#[cfg(test)]
mod test {
    use dam::context_tools::*;
    use dam::simulation::{InitializationOptions, ProgramBuilder, RunOptions};
    use dam::utility_contexts::*;
    use half::f16;
    use ramulator_wrapper::RamulatorWrapper;

    use super::{Memory, RamulatorContext, ReadBundle, WriteBundle, ADDR_OFFSET};
    use crate::ramulator::access::MemoryData;

    #[test]
    fn ramulator_e2e_small() {
        const MEM_SIZE: usize = 32;

        let mut parent = ProgramBuilder::default();

        let config_file =
            "/home/ubuntu/dam_ramulator/external/ramulator2_wrapper/configs/hbm2.yaml";
        let mut mem_context = RamulatorContext::new(config_file, (1u32, 1u32), Some(Memory::new()));

        let (addr_snd, addr_rcv) = parent.unbounded();
        let (data_snd, data_rcv) = parent.unbounded::<MemoryData>();
        let (ack_snd, ack_rcv) = parent.unbounded::<bool>();
        let addrs = || (0..(MEM_SIZE as u64)).map(|x| x * ADDR_OFFSET);
        parent.add_child(GeneratorContext::new(addrs, addr_snd));
        parent.add_child(GeneratorContext::new(
            || (0..(MEM_SIZE as u32)).map(|x| MemoryData::F16([f16::from_f32(x as f32); 16])),
            data_snd,
        ));

        mem_context.add_writer(WriteBundle {
            data: Box::new(data_rcv),
            addr: Box::new(addr_rcv),
            ack: Box::new(ack_snd),
        });

        let (raddr_snd, raddr_rcv) = parent.unbounded();
        let (rdata_snd, rdata_rcv) = parent.unbounded::<MemoryData>();
        // let (size_snd, size_rcv) = parent.unbounded();

        let mut read_ctx = FunctionContext::new();
        raddr_snd.attach_sender(&read_ctx);
        // size_snd.attach_sender(&read_ctx);
        ack_rcv.attach_receiver(&read_ctx);
        read_ctx.set_run(move |time| {
            for iter in 0..MEM_SIZE {
                // Wait for an ack to be received
                ack_rcv.dequeue(time).unwrap();

                raddr_snd
                    .enqueue(
                        time,
                        ChannelElement {
                            time: time.tick() + 1,
                            data: ADDR_OFFSET * (iter as u64),
                        },
                    )
                    .unwrap();
            }
        });
        parent.add_child(read_ctx);

        let (resp_addr_snd, resp_addr_rcv) = parent.unbounded::<u64>();
        mem_context.add_reader(ReadBundle {
            addr: Box::new(raddr_rcv),
            resp: Box::new(rdata_snd),
            resp_addr: Box::new(resp_addr_snd),
        });

        parent.add_child(mem_context);
        let mut verif_context = FunctionContext::new();
        resp_addr_rcv.attach_receiver(&verif_context);
        rdata_rcv.attach_receiver(&verif_context);
        verif_context.set_run(move |time| {
            let mut received: fxhash::FxHashMap<u64, MemoryData> = Default::default();
            for _ in 0..MEM_SIZE {
                let addr = resp_addr_rcv.dequeue(time).unwrap().data;
                let data = rdata_rcv.dequeue(time).unwrap().data;
                received.insert(addr, data);
                time.incr_cycles(1);
            }
            println!("Received: {:?}", received);
        });

        parent.add_child(verif_context);

        println!("Finished building");

        let executed = parent
            .initialize(InitializationOptions::default())
            .unwrap()
            .run(RunOptions::default());

        println!("Elapsed: {:?}", executed.elapsed_cycles());
    }

    #[test]
    fn read_with_logging() {
        const MEM_SIZE: usize = 32;

        let mut parent = ProgramBuilder::default();

        let config_file =
            "/home/ubuntu/dam_ramulator/external/ramulator2_wrapper/configs/hbm2.yaml";
        // let mut mem_context = RamulatorContext::new(config_file, (5u32, 9u32), None);
        let mut mem_context = RamulatorContext::new(config_file, (5u32, 5u32), None);

        // ========================== Read Bundle 1 =============================
        let (raddr_snd, raddr_rcv) = parent.unbounded();
        let (rdata_snd, rdata_rcv) = parent.unbounded::<MemoryData>();
        let (resp_addr_snd, resp_addr_rcv) = parent.unbounded::<u64>();
        /*
        let addrs = {
            let rand_stride = vec![12u64, 0, 4, 2, 6, 9, 11, 12, 19, 24, 36, 72, 3, 5, 9, 11];
            let addr_iter =
                (0..(MEM_SIZE as u64)).map(move |x| rand_stride[(x % 16) as usize] * ADDR_OFFSET);
            move || addr_iter.clone()
        };
        */
        let addrs = || (0..(MEM_SIZE as u64)).map(|x| 0); //x * ADDR_OFFSET);
        parent.add_child(GeneratorContext::new(addrs, raddr_snd));

        let mut read_ctx = FunctionContext::new();
        rdata_rcv.attach_receiver(&read_ctx);
        resp_addr_rcv.attach_receiver(&read_ctx);
        read_ctx.set_run(move |time| {
            let mut received_addr_time: Vec<(u64, u64, u64)> = vec![];
            for _ in 0..MEM_SIZE {
                let (addr, addr_time) = match resp_addr_rcv.dequeue(time) {
                    Ok(addr) => (addr.data, time.tick().time()),
                    Err(_) => {
                        panic!("Failed to dequeue response address");
                    }
                };
                let data_time = match rdata_rcv.dequeue(time) {
                    Ok(addr) => time.tick().time(),
                    Err(_) => {
                        panic!("Failed to dequeue response data");
                    }
                };
                received_addr_time.push((addr, addr_time, data_time));
                // time.incr_cycles(1);
            }
            println!("Received: {:?}", received_addr_time);
        });
        parent.add_child(read_ctx);

        mem_context.add_reader(ReadBundle {
            addr: Box::new(raddr_rcv),
            resp: Box::new(rdata_snd),
            resp_addr: Box::new(resp_addr_snd),
        });

        // // ========================== Read Bundle 2 =============================
        // let (raddr_snd2, raddr_rcv2) = parent.unbounded();
        // let (rdata_snd2, rdata_rcv2) = parent.unbounded::<MemoryData>();
        // let (resp_addr_snd2, resp_addr_rcv2) = parent.unbounded::<u64>();
        // /*
        // let addrs2 = {
        //     let rand_stride = vec![12u64, 0, 4, 2, 6, 9, 11, 12, 19, 24, 36, 72, 3, 5, 9, 11];
        //     let addr_iter =
        //         (0..(MEM_SIZE as u64)).map(move |x| rand_stride[(x % 16) as usize] * ADDR_OFFSET);
        //     move || addr_iter.clone()
        // };
        //  */
        // let addrs2 = || (0..(MEM_SIZE as u64)).map(|x| 32); //x * ADDR_OFFSET);
        // parent.add_child(GeneratorContext::new(addrs2, raddr_snd2));

        // let mut read_ctx2 = FunctionContext::new();
        // rdata_rcv2.attach_receiver(&read_ctx2);
        // resp_addr_rcv2.attach_receiver(&read_ctx2);
        // read_ctx2.set_run(move |time| {
        //     let mut received_addr_time: Vec<(u64, u64, u64)> = vec![];
        //     for _ in 0..MEM_SIZE {
        //         let (addr, addr_time) = match resp_addr_rcv2.dequeue(time) {
        //             Ok(addr) => (addr.data, time.tick().time()),
        //             Err(_) => {
        //                 panic!("Failed to dequeue response address");
        //             }
        //         };
        //         let data_time = match rdata_rcv2.dequeue(time) {
        //             Ok(addr) => time.tick().time(),
        //             Err(_) => {
        //                 panic!("Failed to dequeue response data");
        //             }
        //         };
        //         received_addr_time.push((addr, addr_time, data_time));
        //         // time.incr_cycles(1);
        //     }
        //     println!("Received: {:?}", received_addr_time);
        // });
        // parent.add_child(read_ctx2);

        // mem_context.add_reader(ReadBundle {
        //     addr: Box::new(raddr_rcv2),
        //     resp: Box::new(rdata_snd2),
        //     resp_addr: Box::new(resp_addr_snd2),
        // });

        parent.add_child(mem_context);

        println!("Finished building");

        let executed = parent
            .initialize(InitializationOptions::default())
            .unwrap()
            .run(RunOptions::default());

        println!("Elapsed: {:?}", executed.elapsed_cycles());
    }
}
