# Netidx Dbus

Netidx dbus is a bridge from dbus to netidx. The netidx-dbus daemon will use dbus introspection to discover the tree of objects, interfaces, and properties in dbus, and then it will publish that information to netidx. Properties are modeled as simple netidx values, you can subscribe to them like any other value, and if the property value changes (and the owner of the connection follows the specification and notifies about the change) then the netidx value will update. Methods are modeled as netidx rpcs, calling the netidx rpc causes the dbus method to be called, types are translated in both directions automaticaly.

At the top level netidx-dbus publishes the 'connections' to dbus that don't start with a :, dbus activation support is planned but not currently implemented. Under each connection is a tree of objects, and a special "interfaces" subtree if the object supports any interfaces. Under interfaces, there is a subtree for each interface an object supports. Under each interface is a subtree, "properties" containing all the properties exposed by that interface, and a subtree "methods" containing all the methods exposed by that interface. Signals support is planned but not currently implemented.

```
org.Some.Connection.To.Dbus
|
 \  
  SomeChildObject
   \ ...
  | 
  interfaces (interfaces supported by org.Some.Connection.To.Dbus:/)
   \
    org.Somewhere.Interface
     \
      properties
       \ ... all the properties in org.Somewhere.Interface
      |
      methods
       \ ... all the methods in org.Somewhere.Interface
```
