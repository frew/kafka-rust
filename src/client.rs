//! Kafka Client
//!
//! Primary module of this library.
//!
//! Provides implementation for `KafkaClient` which is used to interact with Kafka

use error::{Result, Error};
use utils;
use protocol;
use connection::KafkaConnection;
use codecs::{ToByte, FromByte};
use std::collections::HashMap;
use std::io::Cursor;
use std::io::Read;

const CLIENTID: &'static str = "kafka-rust";
const DEFAULT_TIMEOUT: i32 = 120; // seconds


/// Client struct.
///
/// It keeps track of brokers and topic metadata
///
/// Implements methods described by Kafka Protocol (https://cwiki.apache.org/confluence/display/KAFKA/A+Guide+To+The+Kafka+Protocol)
///
/// # Examples
///
/// ```no_run
/// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
/// let res = client.load_metadata_all();
/// ```
///
/// You will have to load metadata before making any other request.
#[derive(Default, Debug)]
pub struct KafkaClient {
    clientid: String,
    timeout: i32,
    hosts: Vec<String>,
    correlation: i32,
    conns: HashMap<String, KafkaConnection>,
    /// HashMap where `topic` is the key and list of `partitions` is the value
    pub topic_partitions: HashMap<String, Vec<i32>>,
    topic_brokers: HashMap<String, String>,
    topic_partition_curr: HashMap<String, i32>
}

impl KafkaClient {
    /// Create a new instance of KafkaClient
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// ```
    pub fn new(hosts: Vec<String>) -> KafkaClient {
        KafkaClient { hosts: hosts, clientid: CLIENTID.to_string(),
                      timeout: DEFAULT_TIMEOUT, ..KafkaClient::default()}
    }

    fn get_conn(& mut self, host: &str) -> Result<KafkaConnection> {
        match self.conns.get(host) {
            Some (conn) => return conn.clone(),
            None => {}
        }
        // TODO
        // Keeping this out here since get is causing ownership issues
        // Will refactor once I know better
        self.conns.insert(host.to_string(),
                          try!(KafkaConnection::new(host, self.timeout)));
        self.get_conn(host)
    }

    fn next_id(&mut self) -> i32{
        self.correlation = (self.correlation + 1) % (1i32 << 30);
        self.correlation
    }


    /// Resets and loads metadata for all topics.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// ```
    ///
    pub fn load_metadata_all(&mut self) -> Result<()>{
        self.reset_metadata();
        self.load_metadata(vec!())
    }

    /// Reloads metadata for a list of supplied topics
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata(vec!("my-topic".to_string()));
    /// ```
    ///
    /// returns `Result<(), error::Error>`
    pub fn load_metadata(&mut self, topics: Vec<String>) -> Result<()>{
        let resp = try!(self.get_metadata(topics));

        let mut brokers: HashMap<i32, String> = HashMap::new();
        for broker in resp.brokers {
            brokers.insert(broker.nodeid, format!("{}:{}", broker.host, broker.port));
        }

        for topic in resp.topics {
            self.topic_partitions.insert(topic.topic.clone(), vec!());

            for partition in topic.partitions {
                match brokers.get(&partition.leader) {
                    Some(broker) => {
                        self.topic_partitions.get_mut(&topic.topic).unwrap().push(partition.id);
                        self.topic_brokers.insert(
                            format!("{}-{}", topic.topic, partition.id),
                            broker.clone());
                    },
                    None => {}
                }
            }
        }
        Ok(())
    }

    /// Clears metadata stored in the client. You must load metadata after this call if you want
    /// to use the client
    pub fn reset_metadata(&mut self) {
        self.topic_partitions.clear();
        self.topic_brokers.clear();
    }

    fn get_metadata(&mut self, topics: Vec<String>) -> Result<protocol::MetadataResponse> {
        let correlation = self.next_id();
        for host in self.hosts.to_vec() {
            let req = protocol::MetadataRequest::new(correlation, self.clientid.clone(), topics.to_vec());
            match self.get_conn(&host) {
                Ok(mut conn) => if self.send_request(&mut conn, req).is_ok() {
                    return self.get_response::<protocol::MetadataResponse>(&mut conn);
                },
                Err(_) => {}
            }
        }

        Err(Error::NoHostReachable)
    }

    fn get_broker(&self, topic: &String, partition: &i32) -> Option<String> {
        let key = format!("{}-{}", topic, partition);
        match self.topic_brokers.get(&key) {
            Some(broker) => {
                Some(broker.clone())
            },
            None => None
        }
    }

    fn choose_partition(&mut self, topic: &String) -> Option<i32> {
        match self.topic_partitions.get(topic) {
            Some(partitions) => {
                let plen = partitions.len();
                if plen == 0 {
                    return None;
                }

                let curr = self.topic_partition_curr.entry(topic.clone()).or_insert(0);
                *curr = (*curr+1) % plen as i32;
                Some(*curr)
            },
            None => None
        }

    }

    /// Fetch offsets for a list of topics.
    ///
    /// `time` - Used to ask for all messages before a certain time (ms). There are two special values.
    ///          Specify -1 to receive the latest offset (i.e. the offset of the next coming message)
    ///          and -2 to receive the earliest available offset
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let topics = client.topic_partitions.keys().cloned().collect();
    /// let offsets = client.fetch_offsets(topics, -1);
    /// ```
    /// Returns a hashmap of (topic, PartitionOffset data).
    /// PartitionOffset will contain parition and offset info Or Error code as returned by Kafka.
    pub fn fetch_offsets(&mut self, topics: Vec<String>, time: i64)
        -> Result<HashMap<String, Vec<utils::PartitionOffset>>> {
        let correlation = self.next_id();
        let mut reqs: HashMap<String, protocol::OffsetRequest> = HashMap:: new();

        // Map topic and partition to the corresponding broker
        for topic in topics {
            for p in self.topic_partitions.get(&topic).unwrap_or(&vec!()) {
                self.get_broker(&topic, &p).and_then(|broker| {
                    let entry = reqs.entry(broker.clone()).or_insert(
                                protocol::OffsetRequest::new(correlation, self.clientid.clone()));
                    entry.add(topic.clone(), p.clone(), time);
                    Some(())
                });
            }
        }

        // Call each broker with the request formed earlier
        let mut res: HashMap<String, Vec<utils::PartitionOffset>> = HashMap::new();
        for (host, req) in reqs.iter() {
            let resp = try!(self.send_receive::<protocol::OffsetRequest, protocol::OffsetResponse>(&host, req.clone()));
            for tp in resp.get_offsets() {
                let entry = res.entry(tp.topic).or_insert(vec!());
                entry.push(utils::PartitionOffset{offset:tp.offset, partition: tp.partition});
            }
        }
        Ok(res)
    }

    /// Fetch offset for a topic.
    ///
    /// `time` - Used to ask for all messages before a certain time (ms). There are two special values.
    ///          Specify -1 to receive the latest offset (i.e. the offset of the next coming message)
    ///          and -2 to receive the earliest available offset
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let offsets = client.fetch_topic_offset("my-topic".to_string(), -1);
    /// ```
    /// Returns a hashmap of (topic, PartitionOffset data).
    /// PartitionOffset will contain parition and offset info Or Error code as returned by Kafka.
    pub fn fetch_topic_offset(&mut self, topic: String, time: i64)
        -> Result<HashMap<String, Vec<utils::PartitionOffset>>> {
        self.fetch_offsets(vec!(topic), time)
    }

    /// Fetch messages from Kafka (Multiple topic, partition, offset)
    ///
    /// It takes a vector of `utils:TopicPartitionOffset` and returns a vector of `utils::TopicMessage`
    /// or error::Error
    ///
    /// You can figure out the appropriate partition and offset using client's
    /// `client.topic_partitions` and `client.fetch_topic_offset(topic)`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use kafka::utils;
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let msgs = client.fetch_messages_multi(vec!(utils::TopicPartitionOffset{
    ///                                                 topic: "my-topic".to_string(),
    ///                                                 partition: 0,
    ///                                                 offset: 0
    ///                                                 },
    ///                                             utils::TopicPartitionOffset{
    ///                                                 topic: "my-topic-2".to_string(),
    ///                                                 partition: 0,
    ///                                                 offset: 0
    ///                                             }));
    /// ```
    pub fn fetch_messages_multi(&mut self, input: Vec<utils::TopicPartitionOffset>) -> Result<Vec<utils::TopicMessage>>{

        let correlation = self.next_id();
        let mut reqs: HashMap<String, protocol::FetchRequest> = HashMap:: new();

        // Map topic and partition to the corresponding broker
        for tpo in input {
            self.get_broker(&tpo.topic, &tpo.partition).and_then(|broker| {
                let entry = reqs.entry(broker.clone()).or_insert(
                            protocol::FetchRequest::new(correlation, self.clientid.clone()));
                entry.add(tpo.topic.clone(), tpo.partition.clone(), tpo.offset);
                Some(())
            });
        }

        // Call each broker with the request formed earlier
        let mut res: Vec<utils::TopicMessage> = vec!();
        for (host, req) in reqs.iter() {
            let resp = try!(self.send_receive::<protocol::FetchRequest, protocol::FetchResponse>(&host, req.clone()));
            for tm in resp.get_messages() {
                res.push(tm);
            }
        }
        Ok(res)
    }

    /// Fetch messages from Kafka (Single topic, partition, offset)
    ///
    /// It takes a single topic, parition and offset and return a vector of messages (`utils::TopicMessage`)
    /// or error::Error
    ///
    /// You can figure out the appropriate partition and offset using client's
    /// client.topic_partitions and client.fetch_topic_offset(topic)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let msgs = client.fetch_messages("my-topic".to_string(), 0, 0);
    /// ```
    pub fn fetch_messages(&mut self, topic: String, partition: i32, offset: i64) -> Result<Vec<utils::TopicMessage>>{
        self.fetch_messages_multi(vec!(utils::TopicPartitionOffset{
                                        topic: topic,
                                        partition: partition,
                                        offset: offset
                                        }))
    }

    /// Send a message to Kafka
    ///
    /// You can figure out the appropriate partition and offset using client's
    /// `client.topic_partitions` and `client.fetch_topic_offset(topic)`
    ///
    /// `required_acks` - indicates how many acknowledgements the servers should receive before
    /// responding to the request. If it is 0 the server will not send any response (Not Implemented)
    /// (this is the only case where the server will not reply to a request).
    /// If it is 1, the server will wait the data is written to the local log before sending
    /// a response. If it is -1 the server will block until the message is committed by all
    /// in sync replicas before sending a response. For any number > 1 the server will block
    /// waiting for this number of acknowledgements to occur (but the server will never wait
    /// for more acknowledgements than there are in-sync replicas).
    ///
    /// `timeout` - This provides a maximum time in milliseconds the server can await the
    /// receipt of the number of acknowledgements in `required_acks`
    ///
    /// `input` - A vector of `utils::ProduceMessage`
    ///
    /// # Example
    ///
    /// ```no_run
    /// use kafka::utils;
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let m1 = "a".to_string().into_bytes();
    /// let m2 = "b".to_string().into_bytes();
    /// let req = vec!(utils::ProduceMessage{topic: "my-topic".to_string(), message: m1},
    ///                 utils::ProduceMessage{topic: "my-topic-2".to_string(), message: m2});
    /// println!("{:?}", client.send_messages(1, 100, req));
    /// ```
    /// The return value will contain a vector of topic, partition, offset and error if any
    /// OR error:Error
    pub fn send_messages(&mut self, required_acks: i16, timeout: i32,
                         input: Vec<utils::ProduceMessage>) -> Result<Vec<utils::TopicPartitionOffsetError>> {

        let correlation = self.next_id();
        let mut reqs: HashMap<String, protocol::ProduceRequest> = HashMap::new();

        // Map topic and partition to the corresponding broker
        for pm in input {
            let partition = self.choose_partition(&pm.topic);
            if partition.is_none() {
                continue
            }
            let p = partition.unwrap();
            self.get_broker(&pm.topic, &p).and_then(|broker| {
                let entry = reqs.entry(broker.clone()).or_insert(
                         protocol::ProduceRequest::new(required_acks, timeout, correlation, self.clientid.clone()));
                entry.add(pm.topic.clone(), p.clone(), pm.message.clone());
                Some(())
            });
        }

        // Call each broker with the request formed earlier
        let mut res: Vec<utils::TopicPartitionOffsetError> = vec!();
        for (host, req) in reqs.iter() {
            let resp = try!(self.send_receive::<protocol::ProduceRequest, protocol::ProduceResponse>(&host, req.clone()));
            for tpo in resp.get_response() {
                res.push(tpo);
            }
        }
        Ok(res)
    }

    /// Send a message to Kafka
    ///
    /// You can figure out the appropriate partition and offset using client's
    /// `client.topic_partitions` and `client.fetch_topic_offset(topic)`
    ///
    /// `required_acks` - indicates how many acknowledgements the servers should receive before
    /// responding to the request. If it is 0 the server will not send any response (Not Implemented)
    /// (this is the only case where the server will not reply to a request).
    /// If it is 1, the server will wait the data is written to the local log before sending
    /// a response. If it is -1 the server will block until the message is committed by all
    /// in sync replicas before sending a response. For any number > 1 the server will block
    /// waiting for this number of acknowledgements to occur (but the server will never wait
    /// for more acknowledgements than there are in-sync replicas).
    ///
    /// `timeout` - This provides a maximum time in milliseconds the server can await the
    /// receipt of the number of acknowledgements in `required_acks`
    ///
    /// `message` - A single message as a vector of u8s
    ///
    /// # Example
    ///
    /// ```no_run
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let msgs = client.send_message(1, 100, "my-topic".to_string(), "msg".to_string().into_bytes());
    /// ```
    /// The return value will contain topic, partition, offset and error if any
    /// OR error:Error
    pub fn send_message(&mut self, required_acks: i16, timeout: i32,
                      topic: String, message: Vec<u8>) -> Result<Vec<utils::TopicPartitionOffsetError>> {
        self.send_messages(required_acks, timeout, vec!(utils::ProduceMessage{
            topic: topic,
            message: message
            }))

    }

    /// Commit offset to topic, partition of a consumer group
    ///
    /// It takes a group name and list of `utils::TopicPartitionOffset` and returns `()`
    /// or `error::Error`
    ///
    /// You can figure out the appropriate partition and offset using client's
    /// `client.topic_partitions` and `client.fetch_topic_offset(topic)`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use kafka::utils;
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let resp = client.commit_offsets("my-group".to_string(), vec!(
    ///                 utils::TopicPartitionOffset{topic: "my-topic".to_string(), partition: 0, offset: 100},
    ///                 utils::TopicPartitionOffset{topic: "my-topic".to_string(), partition: 1, offset: 100}));
    /// ```
    pub fn commit_offsets(&mut self, group: String, input: Vec<utils::TopicPartitionOffset>) -> Result<()>{

        let correlation = self.next_id();
        let mut reqs: HashMap<String, protocol::OffsetCommitRequest> = HashMap:: new();

        // Map topic and partition to the corresponding broker
        for tp in input {
            self.get_broker(&tp.topic, &tp.partition).and_then(|broker| {
                let entry = reqs.entry(broker.clone()).or_insert(
                            protocol::OffsetCommitRequest::new(group.clone(), correlation, self.clientid.clone()));
                entry.add(tp.topic.clone(), tp.partition, tp.offset, "".to_string());
                Some(())
            });
        }

        // Call each broker with the request formed earlier
        for (host, req) in reqs.iter() {
            try!(self.send_receive::<protocol::OffsetCommitRequest, protocol::OffsetCommitResponse>(&host, req.clone()));
        }
        Ok(())
    }

    /// Commit offset to topic, partition of a consumer group
    ///
    /// It takes a group name, topic, partition and offset and returns `()`
    /// or `error::Error`
    ///
    /// You can figure out the appropriate partition and offset using client's
    /// `client.topic_partitions` and `client.fetch_topic_offset(topic)`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use kafka::utils;
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let resp = client.commit_offset("my-group".to_string(), "my-topic".to_string(), 0, 100);
    /// ```
    pub fn commit_offset(&mut self, group: String, topic: String,
                         partition: i32, offset: i64) -> Result<()>{
        self.commit_offsets(group, vec!(utils::TopicPartitionOffset{
                topic: topic,
                partition: partition,
                offset: offset}))
    }

    /// Fetch offset for vector of topic, partition of a consumer group
    ///
    /// It takes a group name and list of `utils::TopicPartition` and returns `utils::TopicPartitionOffsetError`
    /// or `error::Error`
    ///
    /// You can figure out the appropriate partition using client's
    /// `client.topic_partitions`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use kafka::utils;
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let resp = client.fetch_group_topics_offset("my-group".to_string(), vec!(
    ///                 utils::TopicPartition{topic: "my-topic".to_string(), partition: 0},
    ///                 utils::TopicPartition{topic: "my-topic".to_string(), partition: 1}));
    /// ```
    pub fn fetch_group_topics_offset(&mut self, group: String, input: Vec<utils::TopicPartition>)
        -> Result<Vec<utils::TopicPartitionOffsetError>>{

        let correlation = self.next_id();
        let mut reqs: HashMap<String, protocol::OffsetFetchRequest> = HashMap:: new();

        // Map topic and partition to the corresponding broker
        for tp in input {
            self.get_broker(&tp.topic, &tp.partition).and_then(|broker| {
                let entry = reqs.entry(broker.clone()).or_insert(
                            protocol::OffsetFetchRequest::new(group.clone(), correlation, self.clientid.clone()));
                entry.add(tp.topic.clone(), tp.partition.clone());
                Some(())
            });
        }

        // Call each broker with the request formed earlier
        let mut res = vec!();
        for (host, req) in reqs.iter() {
            let resp = try!(self.send_receive::<
                            protocol::OffsetFetchRequest, protocol::OffsetFetchResponse>(&host, req.clone()));
            let o = resp.get_offsets();
            for tpo in o {
                res.push(tpo);
            }
        }
        Ok(res)
    }

    /// Fetch offset for all partitions of a topic of a consumer group
    ///
    /// It takes a group name and a topic and returns `utils::TopicPartitionOffsetError`
    /// or `error::Error`
    ///
    /// You can figure out the appropriate partition using client's
    /// `client.topic_partitions`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use kafka::utils;
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let resp = client.fetch_group_topic_offset("my-group".to_string(),"my-topic".to_string());
    /// ```
    pub fn fetch_group_topic_offset(&mut self, group: String, topic: String)
        -> Result<Vec<utils::TopicPartitionOffsetError>> {
        let tps = self.topic_partitions.get(&topic)
                        .unwrap()
                        .iter()
                        .map(|p| utils::TopicPartition{topic: topic.clone(), partition: p.clone()})
                        .collect();
        self.fetch_group_topics_offset(group, tps)

    }

    /// Fetch offset for all partitions of all topics of a consumer group
    ///
    /// It takes a group name and returns `utils::TopicPartitionOffsetError`
    /// or `error::Error`
    ///
    /// You can figure out the topics using client's
    /// `client.topic_partitions`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use kafka::utils;
    /// let mut client = kafka::client::KafkaClient::new(vec!("localhost:9092".to_string()));
    /// let res = client.load_metadata_all();
    /// let resp = client.fetch_group_offset("my-group".to_string());
    /// ```
    pub fn fetch_group_offset(&mut self, group: String)
        -> Result<Vec<utils::TopicPartitionOffsetError>> {
        let mut tps = vec!();
        for (topic, partitions) in self.topic_partitions.iter() {
            for p in partitions {
                tps.push(utils::TopicPartition{topic: topic.clone(), partition: p.clone()})
            }
        }
        self.fetch_group_topics_offset(group, tps)

    }

    fn send_receive<T: ToByte, V: FromByte>(&mut self, host: &str, req: T) -> Result<V::R> {
        let mut conn = try!(self.get_conn(&host));
        try!(self.send_request(&mut conn, req));
        self.get_response::<V>(&mut conn)
    }

    fn send_request<T: ToByte>(&self, conn: &mut KafkaConnection, request: T) -> Result<usize>{
        let mut buffer = vec!();
        try!(request.encode(&mut buffer));

        let mut s = vec!();
        try!((buffer.len() as i32).encode(&mut s));
        for byte in buffer.iter() { s.push(*byte); }

        conn.send(&s)
    }

    fn get_response<T: FromByte>(&self, conn:&mut KafkaConnection) -> Result<T::R>{
        let mut v: Vec<u8> = vec!();
        let _ = conn.read(4, &mut v);

        let size = try!(i32::decode_new(&mut Cursor::new(v)));

        let mut resp: Vec<u8> = vec!();
        let _ = try!(conn.read(size as u64, &mut resp));

        T::decode_new(&mut Cursor::new(resp))
    }

}
