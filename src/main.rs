use anyhow::Result;
use futures::{channel::oneshot, future, prelude::*};
use fxhash::FxHashMap;
use log::warn;
use netidx::{
    path::Path,
    publisher::{BindCfg, Publisher, Val},
    subscriber::Value,
};
use netidx_tools_core::ClientParams;
use std::collections::{HashMap, HashSet};
use structopt::StructOpt;
use tokio::{sync::broadcast, task};
use zbus::{
    fdo::{DBusProxy, IntrospectableProxy, PropertiesProxy},
    xml, Connection,
};
use zbus_names::{InterfaceName, OwnedBusName, OwnedInterfaceName};
use zvariant::{OwnedObjectPath, OwnedValue};

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

async fn introspect(con: &Connection, name: &OwnedBusName) -> Result<xml::Node> {
    let int = IntrospectableProxy::builder(con)
        .path("/")?
        .destination(name)?
        .build()
        .await?;
    let s = int.introspect().await?;
    Ok(xml::Node::from_reader(s.as_bytes())?)
}

#[derive(Debug, Clone)]
struct BusPath {
    connection: OwnedBusName,
    path: OwnedObjectPath,
}

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

fn dbus_value_to_netidx_value(v: &zvariant::Value) -> Value {
    use zvariant::Value as ZValue;
    match v {
        ZValue::U8(i) => Value::U32(*i as u32),
        ZValue::Bool(b) if *b => Value::True,
        ZValue::Bool(b) => Value::False,
        ZValue::I16(i) => Value::I32(*i as i32),
        ZValue::U16(i) => Value::U32(*i as u32),
        ZValue::I32(i) => Value::I32(*i),
        ZValue::U32(i) => Value::U32(*i),
        ZValue::I64(i) => Value::I64(*i),
        ZValue::U64(i) => Value::U64(*i),
        ZValue::F64(i) => Value::F64(*i),
        ZValue::Str(s) => Value::from(s.as_str()),
        ZValue::Signature(s) => Value::from(s.as_str()),
        ZValue::ObjectPath(p) => Value::from(p.as_str()),
        ZValue::Value(v) => dbus_value_to_netidx_value(&*v),
        ZValue::Array(a) => Value::from(
            a.get()
                .into_iter()
                .map(dbus_value_to_netidx_value)
                .collect::<Vec<_>>(),
        ),
        ZValue::Dict(_) => Value::from("<dict>"),
        ZValue::Structure(s) => Value::from(
            s.fields()
                .into_iter()
                .map(dbus_value_to_netidx_value)
                .collect::<Vec<_>>(),
        ),
        ZValue::Maybe(o) => match o.inner() {
            None => Value::Null,
            Some(v) => dbus_value_to_netidx_value(v),
        },
        ZValue::Fd(_) => Value::from("<fd>"),
    }
}

struct Object {
    stop: broadcast::Sender<()>,
    children: Vec<Object>,
}

impl Object {
    async fn publish_properties(
        base: Path,
        publisher: Publisher,
        dbus_path: BusPath,
        dbus: Connection,
        node: xml::Node,
        stop: broadcast::Receiver<()>,
    ) -> Result<()> {
        let proxy = PropertiesProxy::builder(&dbus)
            .destination(&dbus_path.connection)?
            .path(&dbus_path.path)?
            .build()
            .await?;
        let changes = proxy.receive_properties_changed().await?;
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
        Ok(())
    }

    fn new(
        base: &Path,
        publisher: &Publisher,
        dbus_path: &BusPath,
        dbus: &Connection,
        node: &xml::Node,
    ) -> Result<Object> {
        let (stop, _) = broadcast::channel(1);
        if node
            .interfaces()
            .iter()
            .any(|i| i.name() == "org.freedesktop.DBus.Properties")
        {
            let base = base.clone();
            let publisher = publisher.clone();
            let dbus_path = dbus_path.clone();
            let dbus = dbus.clone();
            let stop = stop.subscribe();
            let node = node.clone();
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
                Self::new(&base, publisher, dbus_path, dbus, c)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Object { stop, children })
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

async fn proxy_bus_name(con: Connection, publisher: Publisher, base: Path, name: OwnedBusName) {
    let api = match introspect(&con, &name).await {
        Ok(a) => a,
        Err(e) => {
            warn!("failed to introspect {}, {}", &name, e);
            return;
        }
    };
    print_obj(&api, &name, "/");
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
    let mut names = names
        .into_iter()
        .map(|name| {
            let jh = task::spawn(proxy_bus_name(
                con.clone(),
                publisher.clone(),
                opts.netidx_base.append(name.as_str()),
                name.clone(),
            ));
            (name, jh)
        })
        .collect::<HashMap<_, _>>();
    while let Some(sig) = name_changes.next().await {
        if let Ok(up) = sig.args() {
            if up.new_owner.is_none() {
                if let Some(jh) = names.remove(up.name.as_str()) {
                    jh.abort();
                }
            } else if up.old_owner.is_none() && !up.name.starts_with(":") {
                let name = OwnedBusName::from(up.name);
                let jh = task::spawn(proxy_bus_name(
                    con.clone(),
                    publisher.clone(),
                    opts.netidx_base.append(name.as_str()),
                    name.clone(),
                ));
                names.insert(name, jh);
            }
        }
    }
    Ok(())
}
