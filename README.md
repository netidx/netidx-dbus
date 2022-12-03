# Netidx Dbus

Netidx dbus is a bridge from dbus to netidx. The netidx-dbus daemon
will use dbus introspection to discover the tree of connections,
objects, interfaces, properties, and signals in dbus, and then it will
publish that information to netidx. Properties are modeled as simple
netidx values, you can subscribe to them like any other value, and if
the property value changes (and the owner of the connection follows
the specification and notifies about the change) then the netidx value
will update. Writing to a property causes netidx to call the
appropriate dbus method to set the propery value. Methods are modeled
as netidx rpcs, calling the netidx rpc causes the dbus method to be
called, types are translated in both directions automaticaly. Signals
are modeled as non writable properties with an initial value of null;
when a signal happens subscribed clients will receive it's value, but
subscriptions that happen after the signal will not.

netidx-dbus organizes the dbus namespace into a tree organized by
type. At the top level there are two subtrees `activatible` and
`connections`. Activatible lists connections to dbus that can be
automatically started by dbus, but may not be running now. To activate
an activatible service just write anything to it's value in the
activatible subtree. Connections lists services that are currently
connected to dbus. Connected services contain two subtrees, `children`
and `interfaces`. Children contains immediate children of the object,
in this case of the root object of the connection. Interfaces contains
a list of interfaces implemented by the object. Under each interface
are up to three subtrees, `methods`, `properties`, and `signals`,
which will be present if the interface implements any of the
corresponding item. This is unfortunately verbose, but it is necessary
to prevent namespace clashes, and it mirrors the unfortunately verbose
way that dbus thinks about the world.
