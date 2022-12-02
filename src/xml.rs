// This code is based on zbus::xml, but it has been significantly
// modified

use anyhow::Result;
use serde_xml_rs::{from_reader, from_str};
use std::io::Read;

macro_rules! get_vec {
    ($vec:expr, $kind:path) => {
        $vec.iter()
            .filter_map(|e| if let $kind(m) = e { Some(m) } else { None })
            .collect()
    };
}

/// Annotations are generic key/value pairs of metadata.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Annotation {
    pub name: String,
    pub value: String,
}

/// An argument
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Arg {
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub typ: String,
    pub direction: Option<String>,
    #[serde(rename = "annotation", default)]
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
enum MethodElement {
    Arg(Arg),
    Annotation(Annotation),
}

/// A method
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Method {
    pub name: String,

    #[serde(rename = "$value", default)]
    elems: Vec<MethodElement>,
}

impl Method {
    /// Return the method arguments.
    pub fn args(&self) -> Vec<&Arg> {
        get_vec!(self.elems, MethodElement::Arg)
    }

    /// Return the method annotations.
    pub fn _annotations(&self) -> Vec<&Annotation> {
        get_vec!(self.elems, MethodElement::Annotation)
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
enum SignalElement {
    Arg(Arg),
    Annotation(Annotation),
}

/// A signal
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Signal {
    pub name: String,

    #[serde(rename = "$value", default)]
    elems: Vec<SignalElement>,
}

impl Signal {
    /// Return the signal arguments.
    pub fn args(&self) -> Vec<&Arg> {
        get_vec!(self.elems, SignalElement::Arg)
    }

    /// Return the signal annotations.
    pub fn _annotations(&self) -> Vec<&Annotation> {
        get_vec!(self.elems, SignalElement::Annotation)
    }
}

/// A property
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Property {
    pub name: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub access: String,
    #[serde(rename = "annotation", default)]
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
enum InterfaceElement {
    Method(Method),
    Signal(Signal),
    Property(Property),
    Annotation(Annotation),
}

/// An interface
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Interface {
    pub name: String,

    #[serde(rename = "$value", default)]
    elems: Vec<InterfaceElement>,
}

impl Interface {
    /// Returns the interface methods.
    pub fn methods(&self) -> Vec<&Method> {
        get_vec!(self.elems, InterfaceElement::Method)
    }

    /// Returns the interface signals.
    pub fn signals(&self) -> Vec<&Signal> {
        get_vec!(self.elems, InterfaceElement::Signal)
    }

    /// Returns the interface properties.
    pub fn _properties(&self) -> Vec<&Property> {
        get_vec!(self.elems, InterfaceElement::Property)
    }

    /// Return the associated annotations.
    pub fn _annotations(&self) -> Vec<&Annotation> {
        get_vec!(self.elems, InterfaceElement::Annotation)
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "lowercase")]
enum NodeElement {
    Node(Node),
    Interface(Interface),
}

/// A node in the introspection tree
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Node {
    pub name: Option<String>,

    #[serde(rename = "$value", default)]
    elems: Vec<NodeElement>,
}

impl Node {
    /// Parse the introspection XML document from reader.
    pub fn from_reader<R: Read>(reader: R) -> Result<Node> {
        Ok(from_reader(reader)?)
    }

    /// Returns the children nodes.
    pub fn nodes(&self) -> Vec<&Node> {
        get_vec!(self.elems, NodeElement::Node)
    }

    /// Returns the interfaces on this node.
    pub fn interfaces(&self) -> Vec<&Interface> {
        get_vec!(self.elems, NodeElement::Interface)
    }
}

impl std::str::FromStr for Node {
    type Err = anyhow::Error;

    /// Parse the introspection XML document from `s`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(from_str(s)?)
    }
}
