use std::{
    fmt,
    os::unix::{
        io::{AsRawFd, FromRawFd, IntoRawFd, RawFd},
        net::UnixStream,
    },
    sync::Arc,
};

use smallvec::SmallVec;
use wayland_commons::{
    check_for_signature,
    client::{BackendHandle, ClientBackend, InvalidId, ObjectData, WaylandError},
    core_interfaces::WL_DISPLAY_INTERFACE,
    same_interface, AllowNull, Argument, ArgumentType, Interface, Never, ObjectInfo, ProtocolError,
    ANONYMOUS_INTERFACE,
};

use crate::{
    map::{Object, ObjectMap, SERVER_ID_LIMIT},
    socket::{BufferedSocket, Socket},
    wire::{Message, MessageParseError, INLINE_ARGS},
};

#[derive(Clone)]
struct Data {
    client_destroyed: bool,
    server_destroyed: bool,
    user_data: Arc<dyn ObjectData<Backend>>,
    serial: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Id {
    serial: u32,
    id: u32,
    interface: &'static Interface,
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.interface.name, self.id)
    }
}

impl wayland_commons::client::ObjectId for Id {
    fn is_null(&self) -> bool {
        self.id == 0
    }
}

pub struct Handle {
    socket: BufferedSocket,
    map: ObjectMap<Data>,
    last_error: Option<WaylandError>,
    last_serial: u32,
    pending_placeholder: Option<(&'static Interface, u32)>,
    debug: bool,
}

pub struct Backend {
    handle: Handle,
}

impl ClientBackend for Backend {
    type ObjectId = Id;
    type Handle = Handle;
    type InitError = Never;

    fn connect(stream: UnixStream) -> Result<Self, Never> {
        let socket = BufferedSocket::new(unsafe { Socket::from_raw_fd(stream.into_raw_fd()) });
        let mut map = ObjectMap::new();
        map.insert_at(
            1,
            Object {
                interface: &WL_DISPLAY_INTERFACE,
                version: 1,
                data: Data {
                    client_destroyed: false,
                    server_destroyed: false,
                    user_data: Arc::new(DumbObjectData),
                    serial: 0,
                },
            },
        )
        .unwrap();

        let debug = match std::env::var_os("WAYLAND_DEBUG") {
            Some(str) if str == "1" || str == "client" => true,
            _ => false,
        };

        Ok(Backend {
            handle: Handle {
                socket,
                map,
                last_error: None,
                last_serial: 0,
                pending_placeholder: None,
                debug,
            },
        })
    }

    fn connection_fd(&self) -> RawFd {
        self.handle.socket.as_raw_fd()
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.handle.socket.flush()
    }

    fn dispatch_events(&mut self) -> std::io::Result<usize> {
        let mut dispatched = 0;
        loop {
            // Attempt to read a message
            let map = &self.handle.map;
            let message = match self.handle.socket.read_one_message(|id, opcode| {
                map.find(id)
                    .and_then(|o| o.interface.events.get(opcode as usize))
                    .map(|desc| desc.signature)
            }) {
                Ok(msg) => msg,
                Err(MessageParseError::MissingData) | Err(MessageParseError::MissingFD) => {
                    // need to read more data
                    if let Err(e) = self.handle.socket.fill_incoming_buffers() {
                        if e.kind() != std::io::ErrorKind::WouldBlock || dispatched == 0 {
                            return Err(e);
                        } else {
                            break;
                        }
                    }
                    continue;
                }
                Err(MessageParseError::Malformed) => {
                    // malformed error, protocol error
                    self.handle.last_error = Some(WaylandError::Protocol(ProtocolError {
                        code: 0,
                        object_id: 0,
                        object_interface: "",
                        message: "Malformed Wayland message.".into(),
                    }));
                    return Err(nix::errno::Errno::EPROTO.into());
                }
            };

            // We got a message, retrieve its associated object & details
            // These lookups must succeed otherwise we would not have been able to parse this message
            let receiver = self.handle.map.find(message.sender_id).unwrap();
            let message_desc = receiver.interface.events.get(message.opcode as usize).unwrap();

            // Short-circuit display-associated events
            if message.sender_id == 1 {
                self.handle.handle_display_event(message);
                continue;
            }

            // Convert the arguments and create the new object if applicable
            let mut args =
                SmallVec::<[Argument<Id>; INLINE_ARGS]>::with_capacity(message.args.len());
            let mut arg_interfaces = message_desc.arg_interfaces.iter();
            for arg in message.args.into_iter() {
                args.push(match arg {
                    Argument::Array(a) => Argument::Array(a),
                    Argument::Int(i) => Argument::Int(i),
                    Argument::Uint(u) => Argument::Uint(u),
                    Argument::Str(s) => Argument::Str(s),
                    Argument::Fixed(f) => Argument::Fixed(f),
                    Argument::Fd(f) => Argument::Fd(f),
                    Argument::Object(o) => {
                        if o != 0 {
                            // Lookup the object to make the appropriate Id
                            let obj = match self.handle.map.find(o) {
                                Some(o) => o,
                                None => {
                                    self.handle.last_error = Some(WaylandError::Protocol(ProtocolError {
                                        code: 0,
                                        object_id: 0,
                                        object_interface: "",
                                        message: format!("Unknown object {}.", o),
                                    }));
                                    return Err(nix::errno::Errno::EPROTO.into())
                                }
                            };
                            if let Some(next_interface) = arg_interfaces.next() {
                                if !same_interface(next_interface, obj.interface) && !same_interface(next_interface, &ANONYMOUS_INTERFACE){
                                    self.handle.last_error = Some(WaylandError::Protocol(ProtocolError {
                                        code: 0,
                                        object_id: 0,
                                        object_interface: "",
                                        message: format!(
                                            "Protocol error: server sent object {} for interface {}, but it has interface {}.",
                                            o, next_interface.name, obj.interface.name
                                        ),
                                    }));
                                    return Err(nix::errno::Errno::EPROTO.into())
                                }
                            }
                            Argument::Object(Id { id: o, serial: obj.data.serial, interface: obj.interface })
                        } else {
                            Argument::Object(Id { id: 0, serial: 0, interface: &ANONYMOUS_INTERFACE })
                        }
                    }
                    Argument::NewId(new_id) => {
                        // An object should be created
                        let child_interface = match message_desc.child_interface {
                            Some(iface) => iface,
                            None => panic!("Received event {}@{}.{} which creates an object without specifying its interface, this is unsupported.", receiver.interface.name, message.sender_id, message_desc.name),
                        };

                        let child_udata = receiver.data.user_data.clone().make_child(&ObjectInfo {
                            id: new_id,
                            interface: child_interface,
                            version: receiver.version
                        });

                        // if this ID belonged to a now destroyed server object, we can replace it
                        if new_id >= SERVER_ID_LIMIT
                            && self.handle.map.with(new_id, |obj| obj.data.client_destroyed).unwrap_or(false)
                        {
                            self.handle.map.remove(new_id);
                        }

                        let child_obj = Object {
                            interface: child_interface,
                            version: receiver.version,
                            data: Data {
                                client_destroyed: receiver.data.client_destroyed,
                                server_destroyed: false,
                                user_data: child_udata,
                                serial: self.handle.next_serial(),
                            }
                        };

                        let child_id = Id { id: new_id, serial: child_obj.data.serial, interface: child_obj.interface };

                        if let Err(()) = self.handle.map.insert_at(new_id, child_obj) {
                            // abort parsing, this is an unrecoverable error
                            self.handle.last_error = Some(WaylandError::Protocol(ProtocolError {
                                code: 0,
                                object_id: 0,
                                object_interface: "",
                                message: format!(
                                    "Protocol error: server tried to create \
                                    an object \"{}\" with invalid id {}.",
                                    child_interface.name, new_id
                                ),
                            }));
                            return Err(nix::errno::Errno::EPROTO.into())
                        }

                        Argument::NewId(child_id)
                    }
                });
            }

            if self.handle.debug {
                crate::debug::print_dispatched_message(
                    receiver.interface.name,
                    message.sender_id,
                    message_desc.name,
                    &args,
                );
            }

            // If this event is send to an already destroyed object (by the client), swallow it
            if receiver.data.client_destroyed {
                // but close any associated FD to avoid leaking them
                for a in args {
                    if let Argument::Fd(fd) = a {
                        let _ = ::nix::unistd::close(fd);
                    }
                }
                continue;
            }

            // If this event is a destructor, destroy the object
            if message_desc.is_destructor {
                self.handle
                    .map
                    .with(message.sender_id, |obj| {
                        obj.data.server_destroyed = true;
                        obj.data.client_destroyed = true;
                    })
                    .unwrap();
                receiver.data.user_data.destroyed(Id {
                    id: message.sender_id,
                    serial: receiver.data.serial,
                    interface: receiver.interface,
                });
            }

            // At this point, we invoke the user callback
            receiver.data.user_data.event(
                &mut self.handle,
                Id {
                    id: message.sender_id,
                    serial: receiver.data.serial,
                    interface: receiver.interface,
                },
                message.opcode,
                &args,
            );

            dispatched += 1;
        }
        Ok(dispatched)
    }

    fn handle(&mut self) -> &mut Self::Handle {
        &mut self.handle
    }
}

impl BackendHandle<Backend> for Handle {
    fn display_id(&self) -> Id {
        Id { serial: 0, id: 1, interface: &WL_DISPLAY_INTERFACE }
    }

    fn last_error(&self) -> Option<&WaylandError> {
        self.last_error.as_ref()
    }

    fn info(&self, id: Id) -> Result<ObjectInfo, InvalidId> {
        let object = self.get_object(id)?;
        Ok(ObjectInfo { id: id.id, interface: object.interface, version: object.version })
    }

    fn null_id(&mut self) -> Id {
        Id { serial: 0, id: 0, interface: &ANONYMOUS_INTERFACE }
    }

    fn send_request(
        &mut self,
        id: Id,
        opcode: u16,
        args: &[Argument<Id>],
    ) -> Result<(), InvalidId> {
        let object = self.get_object(id)?;
        if object.data.client_destroyed {
            return Err(InvalidId);
        }

        let message_desc = match object.interface.requests.get(opcode as usize) {
            Some(msg) => msg,
            None => {
                panic!("Unknown opcode {} for object {}@{}.", opcode, object.interface.name, id.id);
            }
        };

        if !check_for_signature(message_desc.signature, args) {
            panic!(
                "Unexpected signature for request {}@{}.{}: expected {:?}, got {:?}.",
                object.interface.name, id.id, message_desc.name, message_desc.signature, args
            );
        }

        if self.debug {
            crate::debug::print_send_message(
                object.interface.name,
                id.id,
                message_desc.name,
                &args,
            );
        }

        let mut msg_args = SmallVec::with_capacity(args.len());
        let mut arg_interfaces = message_desc.arg_interfaces.iter();
        for (i, arg) in args.iter().cloned().enumerate() {
            msg_args.push(match arg {
                Argument::Array(a) => Argument::Array(a),
                Argument::Int(i) => Argument::Int(i),
                Argument::Uint(u) => Argument::Uint(u),
                Argument::Str(s) => Argument::Str(s),
                Argument::Fixed(f) => Argument::Fixed(f),
                Argument::NewId(_) => panic!("Request {}@{}.{} creates an object, and must be send using `send_constructor()`.", object.interface.name, id.id, message_desc.name),
                Argument::Fd(f) => Argument::Fd(f),
                Argument::Object(o) => {
                    if o.id != 0 {
                        let object = self.get_object(o)?;
                        let next_interface = arg_interfaces.next().unwrap();
                        if !same_interface(next_interface, object.interface) {
                            panic!("Request {}@{}.{} expects an argument of interface {} but {} was provided instead.", object.interface.name, id.id, message_desc.name, next_interface.name, object.interface.name);
                        }
                    } else {
                        if !matches!(message_desc.signature[i], ArgumentType::Object(AllowNull::Yes)) {
                            panic!("Request {}@{}.{} expects an non-null object argument.", object.interface.name, id.id, message_desc.name);
                        }
                    }
                    Argument::Object(o.id)
                }
            });
        }

        let msg = Message { sender_id: id.id, opcode, args: msg_args };

        if let Err(err) = self.socket.write_message(&msg) {
            self.last_error = Some(WaylandError::Io(err));
        }

        // Handle destruction if relevant
        if message_desc.is_destructor {
            self.map
                .with(id.id, |obj| {
                    obj.data.client_destroyed = true;
                })
                .unwrap();
            object.data.user_data.destroyed(id);
        }

        Ok(())
    }

    fn placeholder_id(&mut self, spec: Option<(&'static Interface, u32)>) -> Id {
        self.pending_placeholder = spec;
        Id { serial: 0, id: 0, interface: spec.map(|(i, _)| i).unwrap_or(&ANONYMOUS_INTERFACE) }
    }

    fn send_constructor(
        &mut self,
        id: Id,
        opcode: u16,
        args: &[Argument<Id>],
        data: Option<Arc<dyn ObjectData<Backend>>>,
    ) -> Result<Id, InvalidId> {
        let object = self.get_object(id)?;
        if object.data.client_destroyed {
            return Err(InvalidId);
        }

        let message_desc = match object.interface.requests.get(opcode as usize) {
            Some(msg) => msg,
            None => {
                panic!("Unknown opcode {} for object {}@{}.", opcode, object.interface.name, id.id);
            }
        };

        if !check_for_signature(message_desc.signature, args) {
            panic!(
                "Unexpected signature for request {}@{}.{}: expected {:?}, got {:?}.",
                object.interface.name, id.id, message_desc.name, message_desc.signature, args
            );
        }

        // Prepare the child object
        let (child_interface, child_version) = if let Some((iface, version)) =
            self.pending_placeholder.take()
        {
            if let Some(child_interface) = message_desc.child_interface {
                if !same_interface(child_interface, iface) {
                    panic!("Wrong placeholder used when sending request {}@{}.{}: expected interface {} but got {}", object.interface.name, id.id, message_desc.name, child_interface.name, iface.name);
                }
                if !(version == object.version) {
                    panic!("Wrong placeholder used when sending request {}@{}.{}: expected version {} but got {}", object.interface.name, id.id, message_desc.name, object.version, version);
                }
            }
            (iface, version)
        } else {
            if let Some(child_interface) = message_desc.child_interface {
                (child_interface, object.version)
            } else {
                panic!("Wrong placeholder used when sending request {}@{}.{}: target interface must be specified for a generic constructor.", object.interface.name, id.id, message_desc.name);
            }
        };

        let child_serial = self.next_serial();

        let child = Object {
            interface: child_interface,
            version: child_version,
            data: Data {
                client_destroyed: false,
                server_destroyed: false,
                user_data: Arc::new(DumbObjectData),
                serial: child_serial,
            },
        };

        let child_id = self.map.client_insert_new(child);

        self.map
            .with(child_id, |obj| {
                obj.data.user_data = data.unwrap_or_else(|| {
                    object.data.user_data.clone().make_child(&ObjectInfo {
                        interface: child_interface,
                        version: child_version,
                        id: child_id,
                    })
                })
            })
            .unwrap();

        // Prepare the message in a debug-compatible way
        let args = args.iter().cloned().map(|arg| {
            if let Argument::NewId(p) = arg {
                if !p.id == 0 {
                    panic!("The newid provided when sending request {}@{}.{} is not a placeholder.", object.interface.name, id.id, message_desc.name);
                }
                Argument::NewId(Id { id: child_id, serial: child_serial, interface: child_interface})
            } else {
                arg
            }
        }).collect::<Vec<_>>();

        if self.debug {
            crate::debug::print_send_message(
                object.interface.name,
                id.id,
                message_desc.name,
                &args,
            );
        }

        // Send the message

        let mut msg_args = SmallVec::with_capacity(args.len());
        let mut arg_interfaces = message_desc.arg_interfaces.iter();
        for (i, arg) in args.into_iter().enumerate() {
            msg_args.push(match arg {
                Argument::Array(a) => Argument::Array(a),
                Argument::Int(i) => Argument::Int(i),
                Argument::Uint(u) => Argument::Uint(u),
                Argument::Str(s) => Argument::Str(s),
                Argument::Fixed(f) => Argument::Fixed(f),
                Argument::NewId(nid) => Argument::NewId(nid.id),
                Argument::Fd(f) => Argument::Fd(f),
                Argument::Object(o) => {
                    if o.id != 0 {
                        let object = self.get_object(o)?;
                        let next_interface = arg_interfaces.next().unwrap();
                        if !same_interface(next_interface, object.interface) {
                            panic!("Request {}@{}.{} expects an argument of interface {} but {} was provided instead.", object.interface.name, id.id, message_desc.name, next_interface.name, object.interface.name);
                        }
                    } else if !matches!(message_desc.signature[i], ArgumentType::Object(AllowNull::Yes)) {
                        panic!("Request {}@{}.{} expects an non-null object argument.", object.interface.name, id.id, message_desc.name);
                    }
                    Argument::Object(o.id)
                }
            });
        }

        let msg = Message { sender_id: id.id, opcode, args: msg_args };

        if let Err(err) = self.socket.write_message(&msg) {
            self.last_error = Some(WaylandError::Io(err));
        }

        // Handle destruction if relevant
        if message_desc.is_destructor {
            self.map
                .with(id.id, |obj| {
                    obj.data.client_destroyed = true;
                })
                .unwrap();
            object.data.user_data.destroyed(id);
        }

        Ok(Id { id: child_id, serial: child_serial, interface: child_interface })
    }

    fn get_data(&self, id: Id) -> Result<Arc<dyn ObjectData<Backend>>, InvalidId> {
        let object = self.get_object(id)?;
        Ok(object.data.user_data)
    }
}

impl Handle {
    fn next_serial(&mut self) -> u32 {
        self.last_serial = self.last_serial.wrapping_add(1);
        self.last_serial
    }

    fn get_object(&self, id: Id) -> Result<Object<Data>, InvalidId> {
        let object = self.map.find(id.id).ok_or(InvalidId)?;
        if object.data.serial != id.serial {
            return Err(InvalidId);
        }
        Ok(object)
    }

    fn handle_display_event(&mut self, message: Message) {
        match message.opcode {
            0 => {
                // wl_display.error
                if let &[Argument::Object(obj), Argument::Uint(code), Argument::Str(ref message)] =
                    &message.args[..]
                {
                    let object = self.map.find(obj);
                    self.last_error = Some(WaylandError::Protocol(ProtocolError {
                        code,
                        object_id: obj,
                        object_interface: object
                            .map(|obj| obj.interface.name)
                            .unwrap_or("<unknown>"),
                        message: message.to_string_lossy().into(),
                    }))
                } else {
                    unreachable!()
                }
            }
            1 => {
                // wl_display.delete_id
                if let &[Argument::Uint(id)] = &message.args[..] {
                    let client_destroyed = self
                        .map
                        .with(id, |obj| {
                            obj.data.server_destroyed = true;
                            obj.data.client_destroyed
                        })
                        .unwrap_or(false);
                    if client_destroyed {
                        self.map.remove(id);
                    }
                } else {
                    unreachable!()
                }
            }
            _ => unreachable!(),
        }
    }
}

struct DumbObjectData;

impl ObjectData<Backend> for DumbObjectData {
    fn make_child(self: Arc<Self>, _child_info: &ObjectInfo) -> Arc<dyn ObjectData<Backend>> {
        unreachable!()
    }

    fn event(
        &self,
        _handle: &mut Handle,
        _object_id: Id,
        _opcode: u16,
        _arguments: &[Argument<Id>],
    ) {
        unreachable!()
    }

    fn destroyed(&self, _object_id: Id) {
        unreachable!()
    }
}
