use std::fmt;
use std::str;
use std::rc::Rc;
use std::io::Cursor;
use std::cell::RefCell;
use std::time::Duration;
use std::net::SocketAddr;

use rand;
use regex::Regex;
use lru_time_cache::LruCache;
use mio::udp::UdpSocket;
use mio::{Token, EventSet, EventLoop, PollOpt};

use relay::{Relay, Processor, ProcessResult};
use util::{Set, handle_every_line, Dict, slice2str, slice2string};
use network::{is_ip, slice2ip4, slice2ip6, str2addr4, alloc_udp_socket};
use network::{NetworkWriteBytes, NetworkReadBytes};

// All communications inside of the domain protocol are carried in a single
// format called a message.  The top level format of message is divided
// into 5 sections (some of which are empty in certain cases) shown below:
//
//     +---------------------+
//     |        Header       |
//     +---------------------+
//     |       Question      | the question for the name server
//     +---------------------+
//     |        Answer       | RRs answering the question
//     +---------------------+
//     |      Authority      | RRs pointing toward an authority
//     +---------------------+
//     |      Additional     | RRs holding additional information
//     +---------------------+
//
// The header section is always present.  The header includes fields that
// specify which of the remaining sections are present, and also specify
// whether the message is a query or a response, a standard query or some
// other opcode, etc.

// The header section format:
//
//                                     1  1  1  1  1  1
//       0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                      ID                       |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |QR|   Opcode  |AA|TC|RD|RA|   Z    |   RCODE   |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                    QDCOUNT                    |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                    ANCOUNT                    |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                    NSCOUNT                    |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
//     |                    ARCOUNT                    |
//     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+

const QTYPE_A    : u16 = 1;
const QTYPE_AAAA : u16 = 28;
const QTYPE_CNAME: u16 = 5;
const QTYPE_NS   : u16 = 2;
const QCLASS_IN  : u16 = 1;
const _QTYPE_ANY  : u16 = 255;

type ResponseRecord = (String, String, u16, u16);
type ResponseHeader = (u16, u16, u16, u16, u16, u16, u16, u16, u16);
pub type Callback = FnMut(&mut Caller, Option<(String, String)>, Option<&str>);

pub trait Caller {
    fn get_id(&self) -> Token;
    fn handle_dns_resolved(&mut self, event_loop: &mut EventLoop<Relay>,
                           Option<(String, String)>, Option<String>)
                           -> ProcessResult<Vec<Token>>;
}

struct DNSResponse {
    hostname: String,
    questions: Vec<(String, u16, u16)>,
    answers: Vec<(String, u16, u16)>,
}

impl fmt::Debug for DNSResponse {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {:?}", self.hostname, self.answers)
    }
}

impl DNSResponse {
    fn new() -> DNSResponse {
        DNSResponse {
            hostname: String::new(),
            questions: Vec::new(),
            answers: Vec::new()
        }
    }
}


#[derive(Debug, Clone, Copy)]
enum HostnameStatus {
    First,
    Second,
}

pub struct DNSResolver {
    token: Option<Token>,
    hosts: Dict<String, String>,
    cache: LruCache<String, String>,
    callers: Dict<Token, Rc<RefCell<Caller>>>,
    hostname_status: Dict<String, HostnameStatus>,
    token_to_hostname: Dict<Token, String>,
    hostname_to_tokens: Dict<String, Set<Token>>,
    sock: Option<UdpSocket>,
    servers: Vec<String>,
    qtypes: Vec<u16>,
}

impl DNSResolver {
    pub fn new(server_list: Option<Vec<String>>, prefer_ipv6: Option<bool>) -> DNSResolver {
        let servers = match server_list {
            Some(servers) => servers,
            None => parse_resolv(),
        };
        let qtypes = match prefer_ipv6 {
            Some(true) => vec![QTYPE_AAAA, QTYPE_A],
            _ => vec![QTYPE_A, QTYPE_AAAA],
        };
        let hosts = parse_hosts();
        let cache_timeout = Duration::new(600, 0);

        DNSResolver {
            token: None,
            servers: servers,
            hosts: hosts,
            cache: LruCache::with_expiry_duration(cache_timeout),
            callers: Dict::new(),
            hostname_status: Dict::new(),
            token_to_hostname: Dict::new(),
            hostname_to_tokens: Dict::new(),
            sock: alloc_udp_socket(),
            qtypes: qtypes,
        }
    }

    pub fn add_caller(&mut self, caller: Rc<RefCell<Caller>>) {
        let token = caller.borrow().get_id();
        self.callers.put(token, caller);
    }

    pub fn remove_caller(&mut self, token: Token) -> bool {
        if let Some(hostname) = self.token_to_hostname.del(&token) {
            self.hostname_to_tokens.get_mut(&hostname).map(|tokens| tokens.del(&token));
            if self.hostname_to_tokens.get(&hostname).unwrap().is_empty() {
                self.hostname_to_tokens.del(&hostname);
                self.hostname_status.del(&hostname);
            }
        }

        self.callers.del(&token).is_some()
    }

    fn send_request(&self, hostname: String, qtype: u16) {
        match self.sock {
            Some(ref sock) => {
                debug!("send query request of {} to servers", &hostname);
                for server in self.servers.iter() {
                    let server = format!("{}:53", server);
                    let addr = SocketAddr::V4(str2addr4(&server).unwrap());
                    let req = build_request(&hostname, qtype).unwrap();
                    sock.send_to(&req, &addr).unwrap_or_else(|e| {
                        error!("send query request failed: {}", e);
                        None
                    });
                }
            }
            None => error!("DNS socket not init"),
        }
    }

    pub fn resolve(&mut self, token: Token, hostname: String) -> (Option<(String, String)>, Option<String>) {
        if hostname.len() == 0 {
            (None, Some("empty hostname".to_string()))
        } else if is_ip(&hostname) {
            (Some((hostname.clone(), hostname.clone())), None)
        } else if self.hosts.has(&hostname) {
            let ip = self.hosts.get(&hostname).unwrap().clone();
            (Some((hostname.clone(), ip)), None)
        } else if self.cache.contains_key(&hostname) {
            let ip = self.cache.get_mut(&hostname).unwrap();
            (Some((hostname.clone(), ip.clone())), None)
        } else if !is_valid_hostname(&hostname) {
            let errmsg = format!("invalid hostname: {}", hostname);
            (None, Some(errmsg))
        } else {
            // if this is the first time that any caller query the hostname
            if !self.hostname_to_tokens.has(&hostname) {
                self.hostname_status.put(hostname.clone(), HostnameStatus::First);
                self.hostname_to_tokens.put(hostname.clone(), Set::new());
            }
            self.hostname_to_tokens[hostname.clone()].add(token);
            self.token_to_hostname.put(token, hostname.clone());

            self.send_request(hostname, self.qtypes[0]);
            (None, None)
        }
    }

    fn call_callback(&mut self, event_loop: &mut EventLoop<Relay>, hostname: String, ip: String) {
        if let Some(tokens) = self.hostname_to_tokens.del(&hostname) {
            for token in tokens.iter() {
                self.token_to_hostname.del(&token);

                let hostname_ip = Some((hostname.clone(), ip.clone()));
                let caller = self.callers.get_mut(token).unwrap();
                if ip.len() > 0 {
                    caller.borrow_mut().handle_dns_resolved(event_loop, hostname_ip, None);
                } else {
                    let errmsg = format!("unknown hostname {}", hostname.clone());
                    caller.borrow_mut().handle_dns_resolved(event_loop, hostname_ip, Some(errmsg));
                }
            }
        }

        self.hostname_status.del(&hostname);
    }

    fn handle_data(&mut self, event_loop: &mut EventLoop<Relay>, data: &[u8]) {
        match parse_response(data) {
            Some(response) => {
                let mut ip = String::new();
                for answer in response.answers.iter() {
                    if (answer.1 == QTYPE_A || answer.1 == QTYPE_AAAA) && answer.2 == QCLASS_IN {
                        ip = answer.0.clone();
                        break;
                    }
                }

                let hostname = response.hostname;
                let hostname_status = match self.hostname_status.get(&hostname) {
                    Some(&HostnameStatus::First) => 1,
                    Some(&HostnameStatus::Second) => 2,
                    _ => 0
                };

                if ip.len() == 0 && hostname_status == 1 {
                    self.hostname_status.put(hostname.clone(), HostnameStatus::Second);
                    self.send_request(hostname, self.qtypes[1]);
                } else if ip.len() > 0 {
                    self.cache.insert(hostname.clone(), ip.clone());
                    self.call_callback(event_loop, hostname, ip);
                } else if hostname_status == 2 {
                    for question in response.questions {
                        if question.1 == self.qtypes[1] {
                            self.call_callback(event_loop, hostname, String::new());
                            break;
                        }
                    }
                }
            }
            None => error!("invalid DNS response"),
        }
    }

    fn do_register(&mut self, event_loop: &mut EventLoop<Relay>, is_reregister: bool) -> bool {
        if let Some(ref sock) = self.sock {
            let token = self.token.unwrap();
            let events = EventSet::readable();
            let pollopts = PollOpt::edge() | PollOpt::oneshot();

            let register_result = if is_reregister {
                event_loop.reregister(sock, token, events, pollopts)
            } else {
                event_loop.register(sock, token, events, pollopts)
            };

            match register_result {
                Err(e) => {
                    error!("register DNS resolver to event_loop failed: {}", e);
                    false
                }
                _ => true,
            }
        } else {
            error!("create UDP socket for DNSResolver failed");
            false
        }
    }

    pub fn register(&mut self, event_loop: &mut EventLoop<Relay>, token: Token) -> bool {
        self.token = Some(token);
        self.sock = alloc_udp_socket();
        self.do_register(event_loop, false)
    }

    pub fn reregister(&mut self, event_loop: &mut EventLoop<Relay>) -> bool {
        self.do_register(event_loop, true)
    }
}

impl Processor for DNSResolver {
    fn process(&mut self, event_loop: &mut EventLoop<Relay>,
               token: Token,
               events: EventSet)
               -> ProcessResult<Vec<Token>> {
        if events.is_error() {
            error!("events error on DNS socket");
            self.sock.take();
            // TODO: maybe should change to reregister
            self.register(event_loop, token);
        } else {
            let mut buf = [0u8; 1024];
            let received = match self.sock {
                Some(ref sock) => {
                    match sock.recv_from(&mut buf) {
                        Ok(Some((len, _addr))) => Some(&buf[..len]),
                        Ok(None) => None,
                        Err(e) => {
                            error!("receive data failed on DNS socket: {}", e);
                            None
                        }
                    }
                }
                None => {
                    error!("DNS socket not init");
                    None
                }
            };

            if let Some(data) = received {
                self.handle_data(event_loop, data);
            }

            self.reregister(event_loop);
        }

        ProcessResult::Success
    }
}

// For detail, see page 7 of RFC 1035
fn build_address(address: &str) -> Option<Vec<u8>> {
    let mut v = vec![];
    let bytes = address.as_bytes();
    for label in bytes.split(|b| *b == '.' as u8) {
        match label.len() {
            0 => continue,
            n if n > 63 => return None,
            n => {
                v.push(n as u8);
                v.extend(label);
            }
        }
    }

    v.push(0);
    Some(v)
}

// For detail, see page 24 of RFC 1035
fn build_request(address: &str, qtype: u16) -> Option<Vec<u8>> {
    let mut r = vec![];
    // The header section:
    //
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |               random request_id               |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     | 0|     0     | 0| 0| 1| 0|   0    |     0     |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                       1                       |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                       0                       |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                       0                       |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                       0                       |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    let request_id = rand::random::<u16>();

    macro_rules! pack {
        (u16, $r:expr, $v:expr) => ( try_opt!($r.put_u16($v)); );
        (u8, $r:expr, $v:expr) => ( try_opt!($r.put_u8($v)); );
    }

    pack!(u16, r, request_id);
    pack!(u8, r, 1);
    pack!(u8, r, 0);
    pack!(u16, r, 1);
    pack!(u16, r, 0);
    pack!(u16, r, 0);
    pack!(u16, r, 0);
    // address
    let addr = try_opt!(build_address(address));
    r.extend(addr);
    // qtype and qclass
    pack!(u16, r, qtype);
    pack!(u16, r, QCLASS_IN);

    Some(r)
}

// RDATA: a variable length string of octets that describes the resource.
//        The format of this information varies according to the TYPE and CLASS
//        of the resource record. For example, the if the TYPE is A
//        and the CLASS is IN, the RDATA field is a 4 octet ARPA Internet address.
fn parse_ip(addrtype: u16, data: &[u8], length: usize, offset: usize) -> Option<String> {
    let ip_part = &data[offset..offset + length];

    let ip = match addrtype {
        QTYPE_A => slice2ip4(ip_part),
        QTYPE_AAAA => slice2ip6(ip_part),
        QTYPE_CNAME | QTYPE_NS => try_opt!(parse_name(data, offset as u16)).1,
        _ => String::from(try_opt!(slice2str(ip_part))),
    };

    Some(ip)
}

// For detail, see page 29 of RFC 1035
fn parse_name(data: &[u8], offset: u16) -> Option<(u16, String)> {
    let mut p = offset as usize;
    let mut l = data[p];
    let mut labels: Vec<String> = Vec::new();

    while l > 0 {
        // if compressed
        if (l & 0b11000000) == 0b11000000 {
            //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
            //    | 1  1|                OFFSET                   |
            //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
            let mut ptr = try_opt!(Cursor::new(&data[p..p + 2]).get_u16());
            ptr &= 0x3FFF;
            let r = try_opt!(parse_name(data, ptr));
            labels.push(r.1);
            p += 2;
            return Some((p as u16 - offset, labels.join(".")));
        } else {
            labels.push(try_opt!(slice2string(&data[(p + 1)..(p + 1 + l as usize)])));
            p += 1 + l as usize;
        }

        l = data[p];
    }

    Some((p as u16 + 1 - offset, labels.join(".")))
}

// For detail, see page 27, 28 of RFC 1035
fn parse_record(data: &[u8], offset: u16, question: bool) -> Option<(u16, ResponseRecord)> {
    let (nlen, name) = try_opt!(parse_name(data, offset));

    // The question section format:
    //
    //                                     1  1  1  1  1  1
    //       0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                                               |
    //     /                     QNAME                     /
    //     /                                               /
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                     QTYPE                     |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //     |                     QCLASS                    |
    //     +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    let res = if question {
        let bytes = &data[(offset + nlen) as usize ..(offset + nlen + 4) as usize];
        let mut record = Cursor::new(bytes);

        let record_type = try_opt!(record.get_u16());
        let record_class = try_opt!(record.get_u16());

        (nlen + 4, (name, String::new(), record_type, record_class))
    //                                    1  1  1  1  1  1
    //      0  1  2  3  4  5  6  7  8  9  0  1  2  3  4  5
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                                               |
    //    /                                               /
    //    /                      NAME                     /
    //    |                                               |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                      TYPE                     |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                     CLASS                     |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                      TTL                      |
    //    |                                               |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    //    |                   RDLENGTH                    |
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--|
    //    /                     RDATA                     /
    //    /                                               /
    //    +--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+--+
    } else {
        let bytes = &data[(offset + nlen) as usize ..(offset + nlen + 10) as usize];
        let mut record = Cursor::new(bytes);

        let record_type = try_opt!(record.get_u16());
        let record_class = try_opt!(record.get_u16());
        let _record_ttl = try_opt!(record.get_u32());
        let record_rdlength = try_opt!(record.get_u16());

        // RDATA
        let ip = try_opt!(parse_ip(record_type,
                                   data,
                                   record_rdlength as usize,
                                   (offset + nlen + 10) as usize));

        (nlen + 10 + record_rdlength, (name, ip, record_type, record_class))
    };

    Some(res)
}

fn parse_header(data: &[u8]) -> Option<ResponseHeader> {
    if data.len() < 12 {
        return None;
    }

    let mut header = Cursor::new(data);

    macro_rules! unpack {
        (u16, $r:expr) => ( try_opt!($r.get_u16()); );
        (u8, $r:expr) => ( try_opt!($r.get_u8()); );
    }

    let id      = unpack!(u16, header);
    let byte3   = unpack!(u8, header);
    let byte4   = unpack!(u8, header);
    let qdcount = unpack!(u16, header);
    let ancount = unpack!(u16, header);
    let nscount = unpack!(u16, header);
    let arcount = unpack!(u16, header);
    let qr      = (byte3 & 0b10000000) as u16;
    let tc      = (byte3 & 0b00000010) as u16;
    let ra      = (byte4 & 0b00000010) as u16;
    let rcode   = (byte4 & 0b00001111) as u16;

    Some((id, qr, tc, ra, rcode, qdcount, ancount, nscount, arcount))
}

fn parse_records(data: &[u8], offset: u16, count: u16, question: bool) -> Option<(u16, Vec<ResponseRecord>)> {
    let mut records: Vec<ResponseRecord> = Vec::new();
    let mut offset = offset;

    for _i in 0..count {
        let (len, record) = try_opt!(parse_record(data, offset, question));
        offset += len;
        records.push(record);
    }

    Some((offset, records))
}

fn parse_response(data: &[u8]) -> Option<DNSResponse> {
    if data.len() < 12 {
        return None;
    }

    parse_header(data).and_then(|header| {
        let (_id, _qr, _tc, _ra, _rcode, qdcount, ancount, _nscount, _arcount) = header;

        let offset = 12u16;
        let (offset, qds) = try_opt!(parse_records(data, offset, qdcount, true));
        let (_offset, ans) = try_opt!(parse_records(data, offset, ancount, false));
        // We don't need to parse the authority records and the additional records
        let (_offset, _nss) = try_opt!(parse_records(data, _offset, _nscount, false));
        let (_offset, _ars) = try_opt!(parse_records(data, _offset, _arcount, false));

        let mut response = DNSResponse::new();
        if qds.len() > 0 {
            response.hostname = qds[0].0.clone();
        }
        for an in qds {
            response.questions.push((an.1, an.2, an.3))
        }
        for an in ans {
            response.answers.push((an.1, an.2, an.3))
        }

        Some(response)
    })
}

fn parse_resolv() -> Vec<String> {
    let mut servers = vec![];

    handle_every_line("/etc/resolv.conf", &mut |line| {
        if line.starts_with("nameserver") {
            if let Some(server) = line.split_whitespace().nth(1) {
                if is_ip(server) {
                    servers.push(server.to_string());
                }
            }
        }
    });

    if servers.len() == 0 {
        servers = vec!["8.8.4.4".to_string(),
                       "8.8.8.8".to_string()];
    }

    servers
}

fn parse_hosts() -> Dict<String, String> {
    let mut hosts = Dict::new();

    handle_every_line("/etc/hosts", &mut |line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() > 0 {
            let ip = parts[0];
            if is_ip(ip) {
                for hostname in parts[1..].iter() {
                    if hostname.len() > 0 {
                        hosts.put(hostname.to_string(), ip.to_string());
                    }
                }
            }
        }
    });

    hosts.put("localhost".to_string(),
              "127.0.0.1".to_string());

    hosts
}

// For detail, see page 7 of RFC 1035
fn is_valid_hostname(hostname: &str) -> bool {
    if hostname.len() > 255 {
        return false;
    }

    lazy_static! {
        static ref RE: Regex = Regex::new(r"[A-Za-z\d-]{1,63}$").unwrap();
    }

    let hostname = hostname.trim_right_matches('.');
    hostname.as_bytes()
        .split(|c| *c == b'.')
        .all(|s| {
            let s = slice2str(s).unwrap_or("");
            s.len() > 0 && !s.starts_with("-") && !s.ends_with("-") && RE.is_match(s)
        })
}

#[cfg(test)]
mod test {
    use asyncdns;

    #[test]
    fn parse_response() {
        let data: &[u8] = &[
            0x0d, 0x0d, 0x81, 0x80, 0x00, 0x01, 0x00, 0x04,
            0x00, 0x05, 0x00, 0x00, 0x05, 0x62, 0x61, 0x69,
            0x64, 0x75, 0x03, 0x63, 0x6f, 0x6d, 0x00, 0x00,
            0x01, 0x00, 0x01, 0xc0, 0x0c, 0x00, 0x01, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x36, 0x00, 0x04, 0xb4,
            0x95, 0x84, 0x2f, 0xc0, 0x0c, 0x00, 0x01, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x36, 0x00, 0x04, 0xdc,
            0xb5, 0x39, 0xd9, 0xc0, 0x0c, 0x00, 0x01, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x36, 0x00, 0x04, 0x6f,
            0x0d, 0x65, 0xd0, 0xc0, 0x0c, 0x00, 0x01, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x36, 0x00, 0x04, 0x7b,
            0x7d, 0x72, 0x90, 0xc0, 0x0c, 0x00, 0x02, 0x00,
            0x01, 0x00, 0x01, 0x4f, 0x30, 0x00, 0x06, 0x03,
            0x64, 0x6e, 0x73, 0xc0, 0x0c, 0xc0, 0x0c, 0x00,
            0x02, 0x00, 0x01, 0x00, 0x01, 0x4f, 0x30, 0x00,
            0x06, 0x03, 0x6e, 0x73, 0x37, 0xc0, 0x0c, 0xc0,
            0x0c, 0x00, 0x02, 0x00, 0x01, 0x00, 0x01, 0x4f,
            0x30, 0x00, 0x06, 0x03, 0x6e, 0x73, 0x33, 0xc0,
            0x0c, 0xc0, 0x0c, 0x00, 0x02, 0x00, 0x01, 0x00,
            0x01, 0x4f, 0x30, 0x00, 0x06, 0x03, 0x6e, 0x73,
            0x34, 0xc0, 0x0c, 0xc0, 0x0c, 0x00, 0x02, 0x00,
            0x01, 0x00, 0x01, 0x4f, 0x30, 0x00, 0x06, 0x03,
            0x6e, 0x73, 0x32, 0xc0, 0x0c,
        ];

        assert!(asyncdns::parse_response(data).is_some());
    }
}
