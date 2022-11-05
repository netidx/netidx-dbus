#[macro_use]
extern crate serde_derive;

mod xml;
use anyhow::Result;
use dbus::{
    arg::{ArgType, RefArg},
    nonblock::{Proxy, SyncConnection},
};
use futures::{channel::oneshot, future, prelude::*, select_biased};
use fxhash::FxHashMap;
use log::warn;
use netidx::{
    chars::Chars,
    path::Path,
    publisher::{BindCfg, Publisher, Val},
    subscriber::Value,
};
use netidx_tools_core::ClientParams;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use structopt::StructOpt;
use tokio::task;

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
        default_value = "/local/system/sysfs"
    )]
    netidx_base: Path,
}

async fn introspect(con: &Proxy<'_, Arc<SyncConnection>>) -> Result<xml::Node> {
    let (xml,): (String,) = con
        .method_call("org.DBus.Introspectable", "Introspect", ())
        .await?;
    Ok(xml::Node::from_reader(xml.as_bytes())?)
}

/*
struct Interface {
    base: Path,
    publisher: Publisher,
    conn: Connection,
    object_path: BusPath,
    iface: xml::Interface,
    properties: FxHashMap<String, Val>,
}

impl Interface {
    async fn new(
        conn: Connection,
        publisher: Publisher,
        base: Path,
        object_path: BusPath,
        iface: xml::Interface,
    ) -> Result<Interface> {
        let base = base.append(iface.name());
        if iface.properties().len() > 0 {
            let proxy = PropertiesProxy::builder(&conn)
                .destination(&object_path.connection)?
                .path(&object_path.path)?
                .build()
                .await?;
        }
    }
}
*/

fn dbus_value_to_netidx_value<V: RefArg>(v: &V) -> Value {
    match v.arg_type() {
        ArgType::Byte => Value::from(*v.as_any().downcast_ref::<u8>().unwrap()),
        ArgType::Int16 => Value::from(*v.as_any().downcast_ref::<i16>().unwrap()),
        ArgType::UInt16 => Value::from(*v.as_any().downcast_ref::<u16>().unwrap()),
        ArgType::Int32 => Value::from(*v.as_any().downcast_ref::<i32>().unwrap()),
        ArgType::UInt32 => Value::from(*v.as_any().downcast_ref::<u32>().unwrap()),
        ArgType::Int64 => Value::from(*v.as_any().downcast_ref::<i64>().unwrap()),
        ArgType::UInt64 => Value::from(*v.as_any().downcast_ref::<u64>().unwrap()),
        ArgType::Double => Value::from(*v.as_any().downcast_ref::<f64>().unwrap()),
        ArgType::UnixFd => Value::from("<unix-fd>"),
        ArgType::Boolean => Value::from(*v.as_any().downcast_ref::<bool>().unwrap()),
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
    children: Vec<Object>,
}

impl Object {
    async fn publish_properties(
        base: Path,
        publisher: Publisher,
        dbus_path: BusPath,
        dbus: Connection,
        node: xml::Node,
        mut stop: future::Shared<oneshot::Receiver<()>>,
    ) -> Result<()> {
        let proxy = PropertiesProxy::builder(&dbus)
            .destination(&dbus_path.connection)?
            .path(&dbus_path.path)?
            .build()
            .await?;
        let mut changes = proxy.receive_properties_changed().await?;
        let mut properties = future::join_all(
            node.interfaces()
                .into_iter()
                .map(|i| OwnedInterfaceName::try_from(i.name()))
                .collect::<zbus_names::Result<Vec<OwnedInterfaceName>>>()?
                .into_iter()
                .map(|i| {
                    let proxy = &proxy;
                    let publisher = &publisher;
                    let base = &base;
                    async move {
                        let props = proxy
                            .get_all(i.as_ref())
                            .await?
                            .into_iter()
                            .map(|(name, value)| {
                                let path = base.append(i.as_str()).append(&name);
                                let val = publisher
                                    .publish(path, dbus_value_to_netidx_value(&value.into()))?;
                                Ok((name, val))
                            })
                            .collect::<Result<HashMap<_, _>>>()?;
                        Ok((i, props))
                    }
                }),
        )
        .await
        .into_iter()
        .collect::<Result<FxHashMap<_, _>>>()?;
        loop {
            let mut batch = publisher.start_batch();
            select_biased! {
                _ = stop => break,
                change = changes.select_next_some() => {
                    if let Ok(args) = change.args() {
                        if let Some(intf) = properties.get_mut(args.interface_name.as_str()) {
                            for inv in &args.invalidated_properties {
                                intf.remove(*inv);
                            }
                            for (name, value) in &args.changed_properties {
                                match intf.get(*name) {
                                    Some(val) => val.update(&mut batch, dbus_value_to_netidx_value(value)),
                                    None => {
                                        let path = base.append(args.interface_name.as_str()).append(name);
                                        let val = publisher.publish(path, dbus_value_to_netidx_value(value))?;
                                        intf.insert(String::from(*name), val);
                                    }
                                }
                            }
                            if intf.len() == 0 {
                                properties.remove(args.interface_name.as_str());
                            }
                        }
                    }
                }
                complete => break,
            }
        }
        Ok(())
    }

    fn new(
        base: &Path,
        publisher: &Publisher,
        dbus_path: &BusPath,
        dbus: &Connection,
        node: &xml::Node,
        stop: future::Shared<oneshot::Receiver<()>>,
    ) -> Result<Object> {
        if node
            .interfaces()
            .iter()
            .any(|i| i.name() == "org.freedesktop.DBus.Properties")
        {
            let base = base.clone();
            let publisher = publisher.clone();
            let dbus_path = dbus_path.clone();
            let dbus = dbus.clone();
            let node = node.clone();
            let stop = stop.clone();
            task::spawn(async move {
                if let Err(e) =
                    Self::publish_properties(base, publisher, dbus_path.clone(), dbus, node, stop)
                        .await
                {
                    warn!("properties publisher for {:?} failed {}", dbus_path, e)
                }
            });
        }
        let children = node
            .nodes()
            .into_iter()
            .map(|c| {
                let base = c
                    .name()
                    .map(|n| base.append(n))
                    .unwrap_or_else(|| base.clone());
                Self::new(&base, publisher, dbus_path, dbus, c, stop.clone())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Object { children })
    }
}

fn print_obj(api: &xml::Node, name: &OwnedBusName, path: &str) {
    let interfaces = api.interfaces();
    let path = if let Some(name) = api.name() {
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
            &name,
            &path,
            iface.name(),
            has_properties
        );
    }
    for child in api.nodes() {
        print_obj(child, name, &path)
    }
}

struct ProxiedBusName {
    root: Object,
    stop: oneshot::Sender<()>,
}

impl ProxiedBusName {
    async fn new(
        con: Connection,
        publisher: Publisher,
        base: Path,
        name: OwnedBusName,
    ) -> Result<Self> {
        let (stop, receiver) = oneshot::channel();
        let api = introspect(&con, &name).await?;
        let bus_path = BusPath {
            connection: name.clone(),
            path: OwnedObjectPath::default(),
        };
        let root = Object::new(&base, &publisher, &bus_path, &con, &api, receiver.shared())?;
        Ok(ProxiedBusName { root, stop })
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let opts = Params::from_args();
    let (cfg, auth) = opts.common.load();
    let publisher = Publisher::new(cfg, auth, opts.bind).await?;
    let con = Connection::session().await?;
    let dbus = DBusProxy::new(&con).await?;
    let mut name_changes = dbus.receive_name_owner_changed().await?;
    let names = dbus
        .list_activatable_names()
        .await?
        .into_iter()
        .chain(dbus.list_names().await?.into_iter())
        .filter(|n| !n.starts_with(":"))
        .collect::<HashSet<_>>();
    let mut names = future::join_all(names.into_iter().map(|name| async {
        let r = ProxiedBusName::new(
            con.clone(),
            publisher.clone(),
            opts.netidx_base.clone(),
            name.clone().into(),
        )
        .await;
        (name, r)
    }))
    .await
    .into_iter()
    .filter_map(|(name, r)| match r {
        Ok(o) => Some((name, o)),
        Err(e) => {
            warn!("failed to proxy bus name {}: {}", name, e);
            None
        }
    })
    .collect::<FxHashMap<_, _>>();
    while let Some(sig) = name_changes.next().await {
        if let Ok(up) = sig.args() {
            if up.new_owner.is_none() {
                names.remove(up.name.as_str());
            } else if up.old_owner.is_none() && !up.name.starts_with(":") {
                let name = OwnedBusName::from(up.name);
                let r = ProxiedBusName::new(
                    con.clone(),
                    publisher.clone(),
                    opts.netidx_base.append(name.as_str()),
                    name.clone(),
                )
                .await;
                match r {
                    Err(e) => warn!("failed to proxy bus name {}: {}", name, e),
                    Ok(o) => {
                        names.insert(name, o);
                    }
                }
            }
        }
    }
    Ok(())
}
