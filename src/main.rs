#[macro_use]
extern crate serde_derive;

mod xml;
use anyhow::{anyhow, Result};
use dbus::{
    arg::{self, ArgType, RefArg},
    message::MatchRule,
    nonblock::{
        stdintf::org_freedesktop_dbus::{Properties, PropertiesPropertiesChanged},
        MsgMatch, Proxy, SyncConnection,
    },
    strings, Message,
};
use futures::{
    channel::{mpsc::UnboundedReceiver, oneshot},
    future,
    prelude::*,
    select_biased,
};
use fxhash::FxHashMap;
use log::{error, warn};
use netidx::{
    chars::Chars,
    path::Path,
    publisher::{BindCfg, Publisher},
    subscriber::Value,
};
use netidx_tools_core::ClientParams;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use structopt::StructOpt;
use tokio::task;

// make this an argument?
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(StructOpt, Debug)]
struct Params {
    #[structopt(flatten)]
    common: ClientParams,
    #[structopt(
        short = "b",
        long = "bind",
        help = "configure the bind address",
        default_value = "local"
    )]
    bind: BindCfg,
    #[structopt(
        long = "timeout",
        help = "require subscribers to consume values before timeout (seconds)"
    )]
    timeout: Option<u64>,
    #[structopt(
        long = "netidx-base",
        help = "the base path to publish under",
        default_value = "/local/dbus"
    )]
    netidx_base: Path,
}

async fn introspect(con: &Proxy<'_, Arc<SyncConnection>>) -> Result<xml::Node> {
    let (xml,): (String,) = con
        .method_call("org.freedesktop.DBus.Introspectable", "Introspect", ())
        .await?;
    Ok(xml::Node::from_reader(xml.as_bytes())?)
}

async fn list_names(con: &Proxy<'_, Arc<SyncConnection>>) -> Result<Vec<String>> {
    let (names,): (Vec<String>,) = con
        .method_call("org.freedesktop.DBus", "ListNames", ())
        .await?;
    Ok(names)
}

async fn list_activatable_names(con: &Proxy<'_, Arc<SyncConnection>>) -> Result<Vec<String>> {
    let (names,): (Vec<String>,) = con
        .method_call("org.freedesktop.DBus", "ListActivatableNames", ())
        .await?;
    Ok(names)
}

#[derive(Debug, Clone)]
struct NameOwnerChanged {
    name: String,
    old_owner: Option<String>,
    new_owner: Option<String>,
}

impl arg::ReadAll for NameOwnerChanged {
    fn read(i: &mut arg::Iter) -> Result<Self, arg::TypeMismatchError> {
        let name = i.read()?;
        let old_owner: String = i.read()?;
        let new_owner: String = i.read()?;
        let or_none = |s: String| if s.is_empty() { None } else { Some(s) };
        Ok(NameOwnerChanged {
            name,
            old_owner: or_none(old_owner),
            new_owner: or_none(new_owner),
        })
    }
}

fn dbus_value_to_netidx_value<V: RefArg>(v: &V) -> Value {
    match v.arg_type() {
        ArgType::Byte => Value::from(v.as_u64().unwrap() as u32),
        ArgType::Int16 => Value::from(v.as_i64().unwrap() as i32),
        ArgType::UInt16 => Value::from(v.as_u64().unwrap() as u32),
        ArgType::Int32 => Value::from(v.as_i64().unwrap() as i32),
        ArgType::UInt32 => Value::from(v.as_u64().unwrap() as u32),
        ArgType::Int64 => Value::from(v.as_i64().unwrap()),
        ArgType::UInt64 => Value::from(v.as_u64().unwrap()),
        ArgType::Double => Value::from(v.as_f64().unwrap()),
        ArgType::UnixFd => Value::from("<unix-fd>"),
        ArgType::Boolean => Value::from(v.as_i64().unwrap() == 1),
        ArgType::Invalid => Value::Error(Chars::from("invalid")),
        ArgType::String | ArgType::ObjectPath | ArgType::Signature => {
            Value::from(String::from(v.as_str().unwrap()))
        }
        ArgType::Array | ArgType::DictEntry | ArgType::Variant | ArgType::Struct => Value::from(
            v.as_iter()
                .unwrap()
                .map(|v| dbus_value_to_netidx_value(&v))
                .collect::<Vec<_>>(),
        ),
    }
}

struct Object {
    _children: Vec<Object>,
}

impl Object {
    async fn publish_properties(
        base: Path,
        publisher: Publisher,
        proxy: Proxy<'_, Arc<SyncConnection>>,
        node: xml::Node,
        mut stop: future::Shared<oneshot::Receiver<()>>,
    ) -> Result<()> {
        let (filter, mut changes): (
            MsgMatch,
            UnboundedReceiver<(Message, PropertiesPropertiesChanged)>,
        ) = proxy
            .connection
            .add_match(
                MatchRule::new()
                    .with_sender(proxy.destination.clone().into_static())
                    .with_path(proxy.path.clone().into_static())
                    .with_interface("org.freedesktop.DBus.Properties"),
            )
            .await?
            .stream();
        let cleanup = {
            let connection = proxy.connection.clone();
            || async move {
                let _: std::result::Result<_, _> = connection.remove_match(filter.token()).await;
            }
        };
        let mut properties = future::join_all(node.interfaces().into_iter().map(|i| {
            let proxy = &proxy;
            let publisher = &publisher;
            let base = &base;
            async move {
                let i = i.name.clone();
                let props = proxy
                    .get_all(&i)
                    .await?
                    .into_iter()
                    .map(|(name, value)| {
                        let path = base.append(&i).append(&name);
                        let val = publisher
                            .publish(dbg!(path), dbg!(dbus_value_to_netidx_value(&value)))?;
                        Ok((name, val))
                    })
                    .collect::<Result<HashMap<_, _>>>()?;
                Ok::<_, anyhow::Error>((i, props))
            }
        }))
        .await
        .into_iter()
            .filter_map(|r| match r {
                Ok(vals) => Some(vals),
                Err(e) => {
                    warn!("couldn't proxy properties for interface {}", e);
                    None
                }
            })
        .collect::<FxHashMap<_, _>>();
        dbg!(());
        loop {
            let mut batch = publisher.start_batch();
            select_biased! {
                (_, change) = changes.select_next_some() => {
                    if let Some(intf) = properties.get_mut(&change.interface_name) {
                        for inv in &change.invalidated_properties {
                            intf.remove(inv);
                        }
                        for (name, value) in change.changed_properties {
                            match intf.get(&name) {
                                Some(val) => val.update(&mut batch, dbus_value_to_netidx_value(&value)),
                                None => {
                                    let path = base.append(&change.interface_name).append(&name);
                                    let val = publisher.publish(dbg!(path), dbg!(dbus_value_to_netidx_value(&value)))?;
                                    intf.insert(name, val);
                                }
                            }
                        }
                        if intf.len() == 0 {
                            properties.remove(&change.interface_name);
                        }
                    }
                }
                _ = stop => {
                    cleanup().await;
                    break
                },
                complete => {
                    cleanup().await;
                    break
                },
            }
            batch.commit(None).await
        }
        Ok(())
    }

    fn new(
        base: &Path,
        publisher: &Publisher,
        proxy: Proxy<'static, Arc<SyncConnection>>,
        node: &xml::Node,
        stop: future::Shared<oneshot::Receiver<()>>,
    ) -> Result<Object> {
        if node
            .interfaces()
            .iter()
            .any(|i| i.name.as_str() == "org.freedesktop.DBus.Properties")
        {
            let base = base.clone();
            let publisher = publisher.clone();
            let proxy = proxy.clone();
            let node = node.clone();
            let stop = stop.clone();
            task::spawn(async move {
                let path = proxy.path.clone();
                let dest = proxy.destination.clone();
                match Self::publish_properties(base, publisher, proxy, node, stop).await {
                    Ok(()) => warn!("properties publisher for {}:{} stopped", dest, path),
                    Err(e) => warn!("properties publisher for {}:{} failed {}", dest, path, e),
                }
            });
        }
        let _children = node
            .nodes()
            .into_iter()
            .map(|c| {
                let base = c
                    .name
                    .as_ref()
                    .map(|n| base.append(n))
                    .unwrap_or_else(|| base.clone());
                let path = strings::Path::new(
                    c.name
                        .as_ref()
                        .map(|n| {
                            if &*proxy.path == "/" {
                                format!("/{}", n)
                            } else {
                                format!("{}/{}", proxy.path, n)
                            }
                        })
                        .unwrap_or_else(|| String::from(&*proxy.path)),
                )
                .map_err(|_| anyhow!("invalid path {}", base))?;
                let proxy = Proxy::new(
                    proxy.destination.clone(),
                    path,
                    TIMEOUT,
                    Arc::clone(&proxy.connection),
                );
                Self::new(&base, publisher, proxy, c, stop.clone())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Object { _children })
    }
}

/*
fn print_obj(api: &xml::Node, name: &strings::BusName, path: &str) {
    let interfaces = api.interfaces();
    let path = if let Some(name) = api.name.as_ref() {
        if path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", path, name)
        }
    } else {
        format!("{}", path)
    };
    for iface in &interfaces {
        let has_properties = iface.properties().len() > 0;
        println!(
            "obj {}:{}, interface {}, properties {}",
            &name, &path, &iface.name, has_properties
        );
    }
    for child in api.nodes() {
        print_obj(child, name, &path)
    }
}
*/

struct ProxiedBusName {
    _root: Object,
    _stop: oneshot::Sender<()>,
}

impl ProxiedBusName {
    async fn new(
        con: &Arc<SyncConnection>,
        publisher: Publisher,
        base: Path,
        name: String,
    ) -> Result<Self> {
        let (_stop, receiver) = oneshot::channel();
        let proxy = Proxy::new(name, "/", TIMEOUT, con.clone());
        let api = introspect(&proxy).await?;
        let _root = Object::new(&base, &publisher, proxy, &api, receiver.shared())?;
        Ok(ProxiedBusName { _root, _stop })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let opts = Params::from_args();
    let (cfg, auth) = opts.common.load();
    let (dbus, con) = dbus_tokio::connection::new_session_sync()?;
    task::spawn(async move {
        let res = dbus.await;
        error!("lost connection to dbus {}", res);
    });
    let publisher = Publisher::new(cfg, auth, opts.bind).await?;
    let base = opts.netidx_base.clone();
    let _test = publisher.publish(base.append("hello"), Value::Null)?;
    let dbus = Proxy::new("org.freedesktop.DBus", "/", TIMEOUT, Arc::clone(&con));
    let dbus_signal_match = con
        .add_match(
            MatchRule::new()
                .with_sender("org.freedesktop.DBus")
                .with_path("/")
                .with_type(dbus::MessageType::Signal),
        )
        .await?;
    let token = dbus_signal_match.token();
    let (dbus_signal_match, mut signals) = dbus_signal_match.msg_stream();
    /* I need to work out how to deal with activatable names
    let names = list_activatable_names(&dbus)
        .await?
        .into_iter()
        .chain(list_names(&dbus).await?.into_iter())
        .filter(|n| !n.starts_with(":"))
        .collect::<HashSet<_>>();
    */
    let names = list_names(&dbus)
        .await?
        .into_iter()
        .filter(|n| !n.starts_with(":"))
        .collect::<HashSet<_>>();
    let start_proxying = |name: String| {
        let base = base.append(&name);
        let con = &con;
        let publisher = publisher.clone();
        async move {
            let r = ProxiedBusName::new(con, publisher, base, name.clone()).await;
            match r {
                Ok(o) => Some(o),
                Err(e) => {
                    warn!("failed to proxy bus name {}: {}", name, e);
                    None
                }
            }
        }
    };
    let mut names = future::join_all(
        names
            .into_iter()
            .map(|n| async { (n.clone(), start_proxying(n).await) }),
    )
    .await
    .into_iter()
    .filter_map(|(name, r)| r.map(move |r| (name, r)))
    .collect::<FxHashMap<_, _>>();
    while let Some(msg) = signals.next().await {
        dbg!(&msg);
        match msg.member() {
            None => (),
            Some(m) if &*m == "NameOwnerChanged" => {
                if let Ok(up) = msg.read_all::<NameOwnerChanged>() {
                    if up.new_owner.is_none() {
                        names.remove(up.name.as_str());
                    } else if up.old_owner.is_none() && !up.name.starts_with(":") {
                        if let Some(o) = start_proxying(up.name.clone()).await {
                            names.insert(up.name, o);
                        }
                    }
                }
            }
            /* I need to work out how to deal with activatable names
            Some(m) if &*m == "ActivatableServicesChanged" => {
                for name in list_activatable_names(&dbus).await? {
                    if !names.contains_key(&name) {
                        if let Some(o) = start_proxying(name.clone()).await {
                            names.insert(name, o);
                        }
                    }
                }
            }
             */
            Some(_) => (),
        }
    }
    dbus.connection.remove_match(token).await?;
    drop(dbus_signal_match);
    Ok(())
}
