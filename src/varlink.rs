//! The varlink wire protocol, and the interface that describes any service
//! that speaks it.
//!
//! A message is a JSON object followed by one NUL byte, in both directions.
//! That is the whole of the framing.
//!
//! It is written here rather than taken from a crate because the crate that
//! speaks this protocol is built around generating code from an interface at
//! build time, and here the interface goes the other way: the file is embedded
//! and handed out verbatim, so that what a caller is told the service speaks is
//! the very text the service was built from, and the table of methods next to
//! it is hand written and checked against that file by a test.  Generating one
//! from the other would leave nothing to check.

use std::io::{BufRead, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use detc::{Result, err};

/// The interface that lets a caller ask a service what it speaks.
pub(crate) const SERVICE: &str = "org.varlink.service";

/// Its description, embedded so that `GetInterfaceDescription` cannot answer
/// with anything but what the service was built from.
pub(crate) const SERVICE_IDL: &str = include_str!("../varlink/org.varlink.service.varlink");

/// Skips a flag that is not set, as varlink leaves the false ones out.
fn unset(flag: &bool) -> bool {
    !flag
}

/// A call, as it arrives on the socket.
///
/// `oneway` and `upgrade` are read and reported as unsupported rather than
/// ignored, because a caller that asks for either and is answered as if it had
/// not is left waiting for something that is not coming.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Call {
    pub method: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,

    #[serde(default, skip_serializing_if = "unset")]
    pub more: bool,

    #[serde(default, skip_serializing_if = "unset")]
    pub oneway: bool,

    #[serde(default, skip_serializing_if = "unset")]
    pub upgrade: bool,
}

/// One answer to a call.  A call is answered by one reply, or by a stream of
/// them where every reply but the last one continues.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,

    #[serde(default, skip_serializing_if = "unset")]
    pub continues: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Reply {
    /// The last reply of a call.
    pub(crate) fn last(parameters: Value) -> Self {
        Reply {
            parameters: Some(parameters),
            continues: false,
            error: None,
        }
    }

    /// A reply with more behind it.
    pub(crate) fn more(parameters: Value) -> Self {
        Reply {
            parameters: Some(parameters),
            continues: true,
            error: None,
        }
    }

    /// A failure, which ends the call whatever was sent before it.
    pub(crate) fn failed(error: &str, parameters: Value) -> Self {
        Reply {
            parameters: Some(parameters),
            continues: false,
            error: Some(error.to_string()),
        }
    }
}

/// The two ends of a varlink conversation.
///
/// They are separate because they are not always the same file: a service
/// started by `ssh` reads a pipe and writes another one, while one handed a
/// socket reads and writes the same descriptor.
pub(crate) struct Connection<R, W> {
    input: R,
    output: W,
}

impl<R: BufRead, W: Write> Connection<R, W> {
    pub(crate) fn new(input: R, output: W) -> Self {
        Connection { input, output }
    }

    /// The next message, or `None` when the other end is done.
    ///
    /// A message that is not terminated is an error and not the end of the
    /// conversation, so that a connection cut in the middle of a call is told
    /// apart from one closed between two of them.
    pub(crate) fn read<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        let mut buffer = Vec::new();
        self.input.read_until(0, &mut buffer)?;

        match buffer.pop() {
            None => Ok(None),
            Some(0) => Ok(Some(serde_json::from_slice(&buffer)?)),
            Some(_) => err!("The varlink message was cut before it was terminated"),
        }
    }

    /// What was written, for the tests that read the answers back.
    #[cfg(test)]
    pub(crate) fn into_output(self) -> W {
        self.output
    }

    /// Send a message, and let it out.  Nothing is buffered between calls: the
    /// other end is waiting for every reply as it is produced.
    pub(crate) fn write<T: Serialize>(&mut self, message: &T) -> Result<()> {
        serde_json::to_writer(&mut self.output, message)?;
        self.output.write_all(&[0])?;
        self.output.flush()?;

        Ok(())
    }
}

/// Answer a call of [`SERVICE`], the interface that a service implements so
/// that a caller can ask what it is and what else it speaks.
///
/// `None` means the call was not for that interface, and is for the service
/// itself to answer.  Every interface is given with its description, so that
/// what is introspected and what is served cannot be two different things.
pub(crate) fn service(call: &Call, interfaces: &[(&str, &str)]) -> Option<Reply> {
    let names: Vec<&str> = interfaces.iter().map(|(name, _)| *name).collect();

    Some(
        match call.method.strip_prefix(SERVICE)?.strip_prefix('.')? {
            "GetInfo" => Reply::last(json!({
                "vendor": "The detc project",
                "product": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
                "url": "",
                "interfaces": names,
            })),

            "GetInterfaceDescription" => {
                let wanted = call
                    .parameters
                    .as_ref()
                    .and_then(|parameters| parameters.get("interface"))
                    .and_then(Value::as_str);

                match wanted {
                    None => invalid_parameter("interface"),
                    Some(wanted) => match interfaces.iter().find(|(name, _)| *name == wanted) {
                        Some((_, description)) => {
                            Reply::last(json!({ "description": description }))
                        }
                        None => Reply::failed(
                            "org.varlink.service.InterfaceNotFound",
                            json!({ "interface": wanted }),
                        ),
                    },
                }
            }

            _ => method_not_found(&call.method),
        },
    )
}

/// The method is not one that this service has.
pub(crate) fn method_not_found(method: &str) -> Reply {
    Reply::failed(
        "org.varlink.service.MethodNotFound",
        json!({ "method": method }),
    )
}

/// The parameters of the call are not the ones the method takes.
pub(crate) fn invalid_parameter(parameter: &str) -> Reply {
    Reply::failed(
        "org.varlink.service.InvalidParameter",
        json!({ "parameter": parameter }),
    )
}

/// Serialise into the parameters of a call or of a reply.
pub(crate) fn parameters<T: Serialize>(value: &T) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}

/// Read the parameters of a call, filling in the ones that were left out.
pub(crate) fn take<T: DeserializeOwned>(parameters: Option<Value>) -> Result<T> {
    Ok(serde_json::from_value(
        parameters.unwrap_or_else(|| json!({})),
    )?)
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;

    /// A connection whose input is what the other end said, and whose output is
    /// collected.
    fn connection(input: &str) -> Connection<BufReader<&[u8]>, Vec<u8>> {
        Connection::new(BufReader::new(input.as_bytes()), Vec::new())
    }

    #[test]
    fn a_message_is_a_json_object_and_a_nul() {
        let mut connection = connection("");

        connection
            .write(&Call {
                method: "org.detc.Manager.ListTypes".to_string(),
                more: true,
                ..Call::default()
            })
            .unwrap();

        assert_eq!(
            connection.output,
            b"{\"method\":\"org.detc.Manager.ListTypes\",\"more\":true}\0"
        );
    }

    #[test]
    fn two_messages_that_arrive_together_are_both_seen() {
        let mut connection = connection(
            "{\"method\":\"First\"}\0\
             {\"method\":\"Second\"}\0",
        );

        for expected in ["First", "Second"] {
            let call: Call = connection.read().unwrap().unwrap();
            assert_eq!(call.method, expected);
        }

        assert!(connection.read::<Call>().unwrap().is_none());
    }

    #[test]
    fn a_message_that_is_not_terminated_is_an_error() {
        let error = connection("{\"method\":\"Cut\"}")
            .read::<Call>()
            .unwrap_err()
            .to_string();

        assert!(error.contains("cut"), "{error}");
    }

    #[test]
    fn what_is_not_set_is_not_sent() {
        let mut connection = connection("");
        connection.write(&Reply::last(json!({}))).unwrap();

        assert_eq!(connection.output, b"{\"parameters\":{}}\0");
    }

    #[test]
    fn a_call_of_another_interface_is_not_answered_here() {
        let call = Call {
            method: "org.detc.Manager.ListTypes".to_string(),
            ..Call::default()
        };

        assert!(service(&call, &[]).is_none());
    }

    #[test]
    fn the_service_says_what_it_speaks() {
        let call = Call {
            method: "org.varlink.service.GetInfo".to_string(),
            ..Call::default()
        };

        let reply = service(&call, &[(SERVICE, SERVICE_IDL), ("org.detc.Manager", "")]).unwrap();
        let parameters = reply.parameters.unwrap();

        assert_eq!(parameters["product"], "detc");
        assert_eq!(
            parameters["interfaces"],
            json!([SERVICE, "org.detc.Manager"])
        );
    }

    #[test]
    fn the_description_is_the_one_that_is_served() {
        let call = Call {
            method: "org.varlink.service.GetInterfaceDescription".to_string(),
            parameters: Some(json!({ "interface": SERVICE })),
            ..Call::default()
        };

        let reply = service(&call, &[(SERVICE, SERVICE_IDL)]).unwrap();

        assert_eq!(reply.parameters.unwrap()["description"], SERVICE_IDL);
    }

    #[test]
    fn an_interface_that_is_not_served_has_no_description() {
        let call = Call {
            method: "org.varlink.service.GetInterfaceDescription".to_string(),
            parameters: Some(json!({ "interface": "org.example.Other" })),
            ..Call::default()
        };

        let reply = service(&call, &[(SERVICE, SERVICE_IDL)]).unwrap();

        assert_eq!(
            reply.error.unwrap(),
            "org.varlink.service.InterfaceNotFound"
        );
    }

    #[test]
    fn a_method_the_service_does_not_have_is_reported_as_such() {
        let call = Call {
            method: "org.varlink.service.Whatever".to_string(),
            ..Call::default()
        };

        let reply = service(&call, &[]).unwrap();

        assert_eq!(reply.error.unwrap(), "org.varlink.service.MethodNotFound");
        assert_eq!(
            reply.parameters.unwrap()["method"],
            "org.varlink.service.Whatever"
        );
    }
}
