//! Hosts result from a configuration of the system hosts file

use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use crate::proto::op::Query;
use crate::proto::rr::rdata::PTR;
use crate::proto::rr::{Name, RecordType};
use crate::proto::rr::{RData, Record};
use tracing::warn;

use crate::dns_lru;
use crate::lookup::Lookup;

#[derive(Debug, Default)]
struct LookupType {
    /// represents the A record type
    a: Option<Lookup>,
    /// represents the AAAA record type
    aaaa: Option<Lookup>,
}

/// Configuration for the local hosts file
#[derive(Debug, Default)]
pub struct Hosts {
    /// Name -> RDatas map
    by_name: HashMap<Name, LookupType>,
}

impl Hosts {
    /// Creates a new configuration from the system hosts file,
    /// only works for Windows and Unix-like OSes,
    /// will return empty configuration on others
    #[cfg(any(unix, windows))]
    pub fn new() -> Self {
        read_hosts_conf(hosts_path()).unwrap_or_default()
    }

    /// Creates a default configuration for non Windows or Unix-like OSes
    #[cfg(not(any(unix, windows)))]
    pub fn new() -> Self {
        Hosts::default()
    }

    /// Look up the addresses for the given host from the system hosts file.
    pub fn lookup_static_host(&self, query: &Query) -> Option<Lookup> {
        if self.by_name.is_empty() {
            return None;
        }
        match query.query_type() {
            RecordType::A | RecordType::AAAA => {
                let val = self.by_name.get(query.name())?;

                match query.query_type() {
                    RecordType::A => val.a.clone(),
                    RecordType::AAAA => val.aaaa.clone(),
                    _ => None,
                }
            }
            RecordType::PTR => {
                let ip = query.name().parse_arpa_name().ok()?;

                let ip_addr = ip.addr();
                let records = self
                    .by_name
                    .iter()
                    .filter(|(_, v)| match ip_addr {
                        IpAddr::V4(ip) => match v.a.as_ref() {
                            Some(lookup) => lookup
                                .iter()
                                .any(|r| r.ip_addr().map(|it| it == ip).unwrap_or_default()),
                            None => false,
                        },
                        IpAddr::V6(ip) => match v.aaaa.as_ref() {
                            Some(lookup) => lookup
                                .iter()
                                .any(|r| r.ip_addr().map(|it| it == ip).unwrap_or_default()),
                            None => false,
                        },
                    })
                    .map(|(n, _)| {
                        Record::from_rdata(
                            query.name().clone(),
                            dns_lru::MAX_TTL,
                            RData::PTR(PTR(n.clone())),
                        )
                    })
                    .collect::<Arc<[Record]>>();

                if records.is_empty() {
                    return None;
                }

                Some(Lookup::new_with_max_ttl(query.clone(), records))
            }
            _ => None,
        }
    }

    /// Insert a new Lookup for the associated `Name` and `RecordType`
    pub fn insert(&mut self, name: Name, record_type: RecordType, lookup: Lookup) {
        assert!(record_type == RecordType::A || record_type == RecordType::AAAA);

        let lookup_type = self.by_name.entry(name.clone()).or_default();

        let new_lookup = {
            let old_lookup = match record_type {
                RecordType::A => lookup_type.a.get_or_insert_with(|| {
                    let query = Query::query(name.clone(), record_type);
                    Lookup::new_with_max_ttl(query, Arc::from([]))
                }),
                RecordType::AAAA => lookup_type.aaaa.get_or_insert_with(|| {
                    let query = Query::query(name.clone(), record_type);
                    Lookup::new_with_max_ttl(query, Arc::from([]))
                }),
                _ => {
                    tracing::warn!("unsupported IP type from Hosts file: {:#?}", record_type);
                    return;
                }
            };

            old_lookup.append(lookup)
        };

        // replace the appended version
        match record_type {
            RecordType::A => lookup_type.a = Some(new_lookup),
            RecordType::AAAA => lookup_type.aaaa = Some(new_lookup),
            _ => tracing::warn!("unsupported IP type from Hosts file"),
        }
    }

    /// parse configuration from `src`
    pub fn read_hosts_conf(&mut self, src: impl io::Read) -> io::Result<()> {
        use std::io::{BufRead, BufReader};

        // lines in the src should have the form `addr host1 host2 host3 ...`
        // line starts with `#` will be regarded with comments and ignored,
        // also empty line also will be ignored,
        // if line only include `addr` without `host` will be ignored,
        // the src will be parsed to map in the form `Name -> LookUp`.

        for line in BufReader::new(src).lines() {
            // Remove comments from the line
            let line = line?;
            let line = line.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }

            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 2 {
                continue;
            }
            let addr = if let Ok(a) = IpAddr::from_str(fields[0]) {
                RData::from(a)
            } else {
                warn!("could not parse an IP from hosts file");
                continue;
            };

            for domain in fields.iter().skip(1).map(|domain| domain.to_lowercase()) {
                if let Ok(name) = Name::from_str(&domain) {
                    let record = Record::from_rdata(name.clone(), dns_lru::MAX_TTL, addr.clone());

                    match addr {
                        RData::A(..) => {
                            let query = Query::query(name.clone(), RecordType::A);
                            let lookup = Lookup::new_with_max_ttl(query, Arc::from([record]));
                            self.insert(name.clone(), RecordType::A, lookup);
                        }
                        RData::AAAA(..) => {
                            let query = Query::query(name.clone(), RecordType::AAAA);
                            let lookup = Lookup::new_with_max_ttl(query, Arc::from([record]));
                            self.insert(name.clone(), RecordType::AAAA, lookup);
                        }
                        _ => {
                            warn!("unsupported IP type from Hosts file: {:#?}", addr);
                            continue;
                        }
                    };

                    // TODO: insert reverse lookup as well.
                };
            }
        }

        Ok(())
    }
}

#[cfg(unix)]
fn hosts_path() -> &'static str {
    "/etc/hosts"
}

#[cfg(windows)]
fn hosts_path() -> std::path::PathBuf {
    let system_root =
        std::env::var_os("SystemRoot").expect("Environment variable SystemRoot not found");
    let system_root = Path::new(&system_root);
    system_root.join("System32\\drivers\\etc\\hosts")
}

/// parse configuration from `path`
#[cfg(any(unix, windows))]
pub(crate) fn read_hosts_conf<P: AsRef<Path>>(path: P) -> io::Result<Hosts> {
    use std::fs::File;

    let file = File::open(path)?;
    let mut hosts = Hosts::default();
    hosts.read_hosts_conf(file)?;
    Ok(hosts)
}

#[cfg(any(unix, windows))]
#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn tests_dir() -> String {
        let server_path = env::var("TDNS_WORKSPACE_ROOT").unwrap_or_else(|_| "../..".to_owned());
        format! {"{server_path}/crates/resolver/tests"}
    }

    #[test]
    fn test_read_hosts_conf() {
        let path = format!("{}/hosts", tests_dir());
        let hosts = read_hosts_conf(path).unwrap();

        let name = Name::from_str("localhost").unwrap();
        let rdatas = hosts
            .lookup_static_host(&Query::query(name.clone(), RecordType::A))
            .unwrap()
            .iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<RData>>();

        assert_eq!(rdatas, vec![RData::A(Ipv4Addr::LOCALHOST.into())]);

        let rdatas = hosts
            .lookup_static_host(&Query::query(name, RecordType::AAAA))
            .unwrap()
            .iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<RData>>();

        assert_eq!(
            rdatas,
            vec![RData::AAAA(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1).into())]
        );

        let name = Name::from_str("broadcasthost").unwrap();
        let rdatas = hosts
            .lookup_static_host(&Query::query(name, RecordType::A))
            .unwrap()
            .iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<RData>>();
        assert_eq!(
            rdatas,
            vec![RData::A(Ipv4Addr::new(255, 255, 255, 255).into())]
        );

        let name = Name::from_str("example.com").unwrap();
        let rdatas = hosts
            .lookup_static_host(&Query::query(name, RecordType::A))
            .unwrap()
            .iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<RData>>();
        assert_eq!(rdatas, vec![RData::A(Ipv4Addr::new(10, 0, 1, 102).into())]);

        let name = Name::from_str("a.example.com").unwrap();
        let rdatas = hosts
            .lookup_static_host(&Query::query(name, RecordType::A))
            .unwrap()
            .iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<RData>>();
        assert_eq!(rdatas, vec![RData::A(Ipv4Addr::new(10, 0, 1, 111).into())]);

        let name = Name::from_str("b.example.com").unwrap();
        let rdatas = hosts
            .lookup_static_host(&Query::query(name, RecordType::A))
            .unwrap()
            .iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<RData>>();
        assert_eq!(rdatas, vec![RData::A(Ipv4Addr::new(10, 0, 1, 111).into())]);

        let name = Name::from_str("111.1.0.10.in-addr.arpa.").unwrap();
        let mut rdatas = hosts
            .lookup_static_host(&Query::query(name, RecordType::PTR))
            .unwrap()
            .iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<RData>>();
        rdatas.sort_by_key(|r| r.as_ptr().as_ref().map(|p| p.0.clone()));
        assert_eq!(
            rdatas,
            vec![
                RData::PTR(PTR("a.example.com".parse().unwrap())),
                RData::PTR(PTR("b.example.com".parse().unwrap()))
            ]
        );

        let name = Name::from_str(
            "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.ip6.arpa.",
        )
        .unwrap();
        let rdatas = hosts
            .lookup_static_host(&Query::query(name, RecordType::PTR))
            .unwrap()
            .iter()
            .map(ToOwned::to_owned)
            .collect::<Vec<RData>>();
        assert_eq!(rdatas, vec![RData::PTR(PTR("localhost".parse().unwrap())),]);
    }
}
