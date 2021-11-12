use crate::cached_size::CachedSize;
use crate::message_dyn::MessageDyn;
use crate::reflect::dynamic::map::DynamicMap;
use crate::reflect::dynamic::optional::DynamicOptional;
use crate::reflect::dynamic::repeated::DynamicRepeated;
use crate::reflect::map::ReflectMap;
use crate::reflect::repeated::ReflectRepeated;
use crate::reflect::value::value_ref::ReflectValueMut;
use crate::reflect::ReflectFieldRef;
use crate::reflect::ReflectMapMut;
use crate::reflect::ReflectMapRef;
use crate::reflect::ReflectRepeatedMut;
use crate::reflect::ReflectRepeatedRef;
use crate::reflect::ReflectValueBox;
use crate::reflect::RuntimeFieldType;
use crate::reflect::{FieldDescriptor, RuntimeTypeBox};
use crate::reflect::{MessageDescriptor, ReflectValueRef};
use crate::rt::{
    bytes_size, compute_raw_varint32_size, string_size, tag_size, unexpected_wire_type, value_size,
    value_varint_zigzag_size,
};
use crate::wire_format::WireType;
use crate::Clear;
use crate::CodedInputStream;
use crate::CodedOutputStream;
use crate::Message;
use crate::ProtobufResult;
use crate::UnknownFields;

use super::EnumValueDescriptor;
use crate::descriptor::field_descriptor_proto::Type;
use std::convert::TryInto;

pub(crate) mod map;
pub(crate) mod optional;
pub(crate) mod repeated;

#[derive(Debug, Clone)]
enum DynamicFieldValue {
    Singular(DynamicOptional),
    Repeated(DynamicRepeated),
    Map(DynamicMap),
}

impl DynamicFieldValue {
    fn as_ref(&self) -> ReflectFieldRef {
        match self {
            DynamicFieldValue::Singular(v) => ReflectFieldRef::Optional(v.get()),
            DynamicFieldValue::Repeated(r) => ReflectFieldRef::Repeated(ReflectRepeatedRef::new(r)),
            DynamicFieldValue::Map(m) => ReflectFieldRef::Map(ReflectMapRef::new(m)),
        }
    }

    fn clear(&mut self) {
        match self {
            DynamicFieldValue::Singular(o) => o.clear(),
            DynamicFieldValue::Repeated(r) => r.clear(),
            DynamicFieldValue::Map(m) => m.clear(),
        }
    }
}

impl DynamicFieldValue {
    fn default_for_field(field: &FieldDescriptor) -> DynamicFieldValue {
        match field.runtime_field_type() {
            RuntimeFieldType::Singular(s) => DynamicFieldValue::Singular(DynamicOptional::none(s)),
            RuntimeFieldType::Repeated(r) => DynamicFieldValue::Repeated(DynamicRepeated::new(r)),
            RuntimeFieldType::Map(k, v) => DynamicFieldValue::Map(DynamicMap::new(k, v)),
        }
    }

    /// set default value for singular fields
    fn set_default_for_merge(&mut self, field: &FieldDescriptor) {
        match field.runtime_field_type() {
            RuntimeFieldType::Singular(rtb) => {
                assert!(matches!(self, DynamicFieldValue::Singular(..)));
                if let DynamicFieldValue::Singular(s) = self {
                    match rtb {
                        RuntimeTypeBox::I32 => {
                            s.set(ReflectValueBox::from(0 as i32));
                        }
                        RuntimeTypeBox::I64 => {
                            s.set(ReflectValueBox::from(0 as i64));
                        }
                        RuntimeTypeBox::U32 => {
                            s.set(ReflectValueBox::from(0 as u32));
                        }
                        RuntimeTypeBox::U64 => {
                            s.set(ReflectValueBox::from(0 as u64));
                        }
                        RuntimeTypeBox::F32 => {
                            s.set(ReflectValueBox::from(0 as f32));
                        }
                        RuntimeTypeBox::F64 => {
                            s.set(ReflectValueBox::from(0 as f64));
                        }
                        RuntimeTypeBox::Bool => {
                            s.set(ReflectValueBox::from(false));
                        }
                        RuntimeTypeBox::String => {
                            s.set(ReflectValueBox::from("".to_string()));
                        }
                        RuntimeTypeBox::VecU8 => {
                            s.set(ReflectValueBox::from(Vec::default()));
                        }
                        RuntimeTypeBox::Enum(enum_desc) => {
                            s.set(ReflectValueBox::from(EnumValueDescriptor::new(
                                enum_desc, 0,
                            )));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DynamicMessage {
    pub(crate) descriptor: MessageDescriptor,
    fields: Box<[DynamicFieldValue]>,
    unknown_fields: UnknownFields,
    cached_size: CachedSize,
}

impl DynamicMessage {
    pub(crate) fn new(descriptor: MessageDescriptor) -> DynamicMessage {
        DynamicMessage {
            descriptor,
            fields: Vec::new().into_boxed_slice(),
            unknown_fields: UnknownFields::new(),
            cached_size: CachedSize::new(),
        }
    }

    fn init_fields(&mut self) {
        if self.fields.is_empty() {
            self.fields = self
                .descriptor
                .fields()
                .map(|f| DynamicFieldValue::default_for_field(&f))
                .collect();
        }
    }

    pub(crate) fn get_reflect<'a>(&'a self, field: &FieldDescriptor) -> ReflectFieldRef<'a> {
        assert_eq!(self.descriptor, field.message_descriptor);
        if self.fields.is_empty() {
            ReflectFieldRef::default_for_field(field)
        } else {
            self.fields[field.index].as_ref()
        }
    }

    pub fn clear_field(&mut self, field: &FieldDescriptor) {
        assert_eq!(field.message_descriptor, self.descriptor);
        if self.fields.is_empty() {
            return;
        }

        self.fields[field.index].clear();
    }

    fn check_singular_initialized(&self, rtb: &RuntimeTypeBox, f: &FieldDescriptor) -> bool {
        if let RuntimeTypeBox::Message(_) = rtb {
            if let Some(msg) = f.get_singular(self) {
                return msg.to_message().unwrap().is_initialized_dyn();
            }
        }
        true
    }

    fn check_repeated_initialized(&self, rtb: &RuntimeTypeBox, f: &FieldDescriptor) -> bool {
        if let RuntimeTypeBox::Message(_) = rtb {
            let msg_list = f.get_repeated(self);
            for i in 0..msg_list.len() {
                let msg = msg_list.get(i).to_message().unwrap();
                if !msg.is_initialized_dyn() {
                    return false;
                }
            }
        }
        true
    }

    /// Set all fields to default value
    pub fn set_fields_default(&mut self) {
        self.init_fields();
        let syntax = self.descriptor.file_descriptor_proto().get_syntax();
        if syntax != "proto3" {
            return; // for proto2, default value is unset
        }
        if !self.fields.is_empty() {
            for field_desc in self.descriptor.fields() {
                self.fields[field_desc.index].set_default_for_merge(&field_desc);
            }
        }
    }

    fn clear_oneof_group_fields_except(&mut self, field: &FieldDescriptor) {
        if let Some(oneof) = field.containing_oneof() {
            for next in oneof.fields() {
                if &next == field {
                    continue;
                }
                self.clear_field(&next);
            }
        }
    }

    pub(crate) fn mut_singular_field_or_default<'a>(
        &'a mut self,
        field: &FieldDescriptor,
    ) -> ReflectValueMut<'a> {
        assert_eq!(field.message_descriptor, self.descriptor);
        self.init_fields();
        self.clear_oneof_group_fields_except(field);
        // TODO: reset oneof group fields
        match &mut self.fields[field.index] {
            DynamicFieldValue::Singular(f) => f.mut_or_default(),
            _ => panic!("Not a singular field"),
        }
    }

    pub(crate) fn mut_repeated<'a>(
        &'a mut self,
        field: &FieldDescriptor,
    ) -> ReflectRepeatedMut<'a> {
        assert_eq!(self.descriptor, field.message_descriptor);
        self.init_fields();
        // TODO: reset oneof group fields
        match &mut self.fields[field.index] {
            DynamicFieldValue::Repeated(r) => ReflectRepeatedMut::new(r),
            _ => panic!("Not a repeated field: {}", field),
        }
    }

    pub(crate) fn mut_map<'a>(&'a mut self, field: &FieldDescriptor) -> ReflectMapMut<'a> {
        assert_eq!(field.message_descriptor, self.descriptor);
        self.init_fields();
        // TODO: reset oneof group fields
        match &mut self.fields[field.index] {
            DynamicFieldValue::Map(m) => ReflectMapMut::new(m),
            _ => panic!("Not a map field: {}", field),
        }
    }

    pub(crate) fn set_field(&mut self, field: &FieldDescriptor, value: ReflectValueBox) {
        assert_eq!(field.message_descriptor, self.descriptor);
        self.init_fields();
        // TODO: reset oneof group fields
        match &mut self.fields[field.index] {
            DynamicFieldValue::Singular(s) => s.set(value),
            _ => panic!("Not a singular field: {}", field),
        }
    }

    pub fn downcast_ref(message: &dyn MessageDyn) -> &DynamicMessage {
        <dyn MessageDyn>::downcast_ref(message).unwrap()
    }

    pub fn downcast_mut(message: &mut dyn MessageDyn) -> &mut DynamicMessage {
        <dyn MessageDyn>::downcast_mut(message).unwrap()
    }
}

impl Clear for DynamicMessage {
    fn clear(&mut self) {
        unimplemented!()
    }
}

impl Message for DynamicMessage {
    fn descriptor_by_instance(&self) -> MessageDescriptor {
        self.descriptor.clone()
    }

    fn is_initialized(&self) -> bool {
        for f in self.descriptor.fields() {
            match f.runtime_field_type() {
                RuntimeFieldType::Singular(rtb) => {
                    if !self.check_singular_initialized(&rtb, &f) {
                        return false;
                    }
                }
                RuntimeFieldType::Repeated(rtb) => {
                    if !self.check_repeated_initialized(&rtb, &f) {
                        return false;
                    }
                }
                RuntimeFieldType::Map(_, _) => {
                    unimplemented!()
                }
            }
        }

        true
    }

    fn merge_from(&mut self, is: &mut CodedInputStream) -> ProtobufResult<()> {
        self.set_fields_default();
        let desc = self.descriptor.clone();
        while !is.eof()? {
            let (field, wire_type) = is.read_tag_unpack()?;
            let field_desc = desc
                .get_field_by_number(field)
                .expect("Invalid field number at decoding");
            let field_desc_proto = field_desc.get_proto();
            match field_desc.runtime_field_type() {
                RuntimeFieldType::Singular(rtb) => {
                    let val = match field_desc_proto.get_field_type() {
                        Type::TYPE_DOUBLE => ReflectValueBox::from(is.read_double()?),
                        Type::TYPE_FLOAT => ReflectValueBox::from(is.read_float()?),
                        Type::TYPE_INT64 => ReflectValueBox::from(is.read_int64()?),
                        Type::TYPE_UINT64 => ReflectValueBox::from(is.read_uint64()?),
                        Type::TYPE_INT32 => ReflectValueBox::from(is.read_int32()?),
                        Type::TYPE_FIXED64 => ReflectValueBox::from(is.read_fixed64()?),
                        Type::TYPE_FIXED32 => ReflectValueBox::from(is.read_fixed32()?),
                        Type::TYPE_BOOL => ReflectValueBox::from(is.read_bool()?),
                        Type::TYPE_STRING => ReflectValueBox::from(is.read_string()?),
                        Type::TYPE_GROUP => {
                            unimplemented!()
                        }
                        Type::TYPE_MESSAGE => {
                            assert!(matches!(rtb, RuntimeTypeBox::Message(..)));
                            if let RuntimeTypeBox::Message(msg_desc) = rtb {
                                let mut msg_inst = msg_desc.new_instance();
                                is.incr_recursion()?;
                                is.merge_message_dyn(msg_inst.as_mut())?;
                                is.decr_recursion();
                                ReflectValueBox::from(msg_inst)
                            } else {
                                panic!("Protobuf type and Runtime type mismatch");
                            }
                        }
                        Type::TYPE_BYTES => ReflectValueBox::from(is.read_bytes()?),
                        Type::TYPE_UINT32 => ReflectValueBox::from(is.read_uint32()?),
                        Type::TYPE_ENUM => {
                            assert!(matches!(rtb, RuntimeTypeBox::Enum(..)));
                            if let RuntimeTypeBox::Enum(enum_desc) = rtb {
                                let enum_num = is.read_int32()?;
                                ReflectValueBox::from(EnumValueDescriptor::new(
                                    enum_desc,
                                    enum_num as usize, // FIXME: might unsatisfied
                                ))
                            } else {
                                panic!("Protobuf type and Runtime type mismatch");
                            }
                        }
                        Type::TYPE_SFIXED32 => ReflectValueBox::from(is.read_sfixed32()?),
                        Type::TYPE_SFIXED64 => ReflectValueBox::from(is.read_sfixed64()?),
                        Type::TYPE_SINT32 => ReflectValueBox::from(is.read_sint32()?),
                        Type::TYPE_SINT64 => ReflectValueBox::from(is.read_sint64()?),
                    };
                    self.set_field(&field_desc, val);
                }
                RuntimeFieldType::Repeated(rtb) => {
                    let mut repeated_mut = self.mut_repeated(&field_desc);

                    match field_desc_proto.get_field_type() {
                        Type::TYPE_FLOAT => match wire_type {
                            WireType::WireTypeFixed32 => {
                                repeated_mut.push(ReflectValueBox::from(is.read_float()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<f32> = Vec::default();
                                is.read_repeated_packed_float_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_DOUBLE => match wire_type {
                            WireType::WireTypeFixed64 => {
                                repeated_mut.push(ReflectValueBox::from(is.read_double()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<f64> = Vec::default();
                                is.read_repeated_packed_double_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_INT32 => match wire_type {
                            WireType::WireTypeVarint => {
                                repeated_mut.push(ReflectValueBox::from(is.read_int32()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<i32> = Vec::default();
                                is.read_repeated_packed_int32_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_INT64 => match wire_type {
                            WireType::WireTypeVarint => {
                                repeated_mut.push(ReflectValueBox::from(is.read_int64()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<i64> = Vec::default();
                                is.read_repeated_packed_int64_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_UINT32 => match wire_type {
                            WireType::WireTypeVarint => {
                                repeated_mut.push(ReflectValueBox::from(is.read_uint32()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<u32> = Vec::default();
                                is.read_repeated_packed_uint32_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_UINT64 => match wire_type {
                            WireType::WireTypeVarint => {
                                repeated_mut.push(ReflectValueBox::from(is.read_uint64()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<u64> = Vec::default();
                                is.read_repeated_packed_uint64_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_FIXED32 => match wire_type {
                            WireType::WireTypeFixed32 => {
                                repeated_mut.push(ReflectValueBox::from(is.read_fixed32()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<u32> = Vec::default();
                                is.read_repeated_packed_fixed32_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_FIXED64 => match wire_type {
                            WireType::WireTypeFixed64 => {
                                repeated_mut.push(ReflectValueBox::from(is.read_fixed64()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<u64> = Vec::default();
                                is.read_repeated_packed_fixed64_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_BOOL => match wire_type {
                            WireType::WireTypeVarint => {
                                repeated_mut.push(ReflectValueBox::from(is.read_bool()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<bool> = Vec::default();
                                is.read_repeated_packed_bool_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_STRING => {
                            repeated_mut.push(ReflectValueBox::from(is.read_string()?));
                        }
                        Type::TYPE_GROUP => {
                            unimplemented!();
                        }
                        Type::TYPE_SFIXED32 => match wire_type {
                            WireType::WireTypeFixed32 => {
                                repeated_mut.push(ReflectValueBox::from(is.read_sfixed32()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<i32> = Vec::default();
                                is.read_repeated_packed_sfixed32_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_SFIXED64 => match wire_type {
                            WireType::WireTypeFixed64 => {
                                repeated_mut.push(ReflectValueBox::from(is.read_sfixed64()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<i64> = Vec::default();
                                is.read_repeated_packed_sfixed64_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_SINT32 => match wire_type {
                            WireType::WireTypeVarint => {
                                repeated_mut.push(ReflectValueBox::from(is.read_sint32()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<i32> = Vec::default();
                                is.read_repeated_packed_sint32_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_SINT64 => match wire_type {
                            WireType::WireTypeVarint => {
                                repeated_mut.push(ReflectValueBox::from(is.read_sint64()?));
                            }
                            WireType::WireTypeLengthDelimited => {
                                let mut res_vec: Vec<i64> = Vec::default();
                                is.read_repeated_packed_sint64_into(&mut res_vec)?;
                                for i in res_vec {
                                    repeated_mut.push(ReflectValueBox::from(i));
                                }
                            }
                            _ => return Err(unexpected_wire_type(wire_type)),
                        },
                        Type::TYPE_BYTES => {
                            repeated_mut.push(ReflectValueBox::from(is.read_bytes()?));
                        }
                        Type::TYPE_ENUM => {
                            assert!(matches!(rtb, RuntimeTypeBox::Enum(..)));
                            if let RuntimeTypeBox::Enum(enum_desc) = rtb {
                                let enum_num = is.read_int32()?;
                                let enum_val = ReflectValueBox::from(EnumValueDescriptor::new(
                                    enum_desc,
                                    enum_num.try_into().unwrap(),
                                ));
                                repeated_mut.push(enum_val);
                            } else {
                                panic!("Protobuf type and Runtime type mismatch");
                            }
                        }
                        Type::TYPE_MESSAGE => {
                            assert!(matches!(rtb, RuntimeTypeBox::Message(..)));
                            if let RuntimeTypeBox::Message(msg_desc) = rtb {
                                let mut msg_inst = msg_desc.new_instance();
                                is.merge_message_dyn(msg_inst.as_mut())?;
                                let msg_val = ReflectValueBox::from(msg_inst);
                                repeated_mut.push(msg_val);
                            } else {
                                panic!("Protobuf type and Runtime type mismatch");
                            }
                        }
                    }
                }
                RuntimeFieldType::Map(_, _) => {}
            }
        }
        Ok(())
    }

    fn write_to_with_cached_sizes(&self, os: &mut CodedOutputStream) -> ProtobufResult<()> {
        for field_desc in self.descriptor.fields() {
            let field_number = field_desc.get_proto().get_number() as u32;
            match field_desc.runtime_field_type() {
                RuntimeFieldType::Singular(rtb) => {
                    if let Some(v) = field_desc.get_singular(self) {
                        if v.is_non_zero() {
                            // ignore default value
                            singular_write_to(
                                &rtb,
                                &field_desc.get_proto().get_field_type(),
                                field_number,
                                &v,
                                os,
                            )?;
                        }
                    }
                }
                RuntimeFieldType::Repeated(rtb) => {
                    let repeated = field_desc.get_repeated(self);
                    for i in 0..repeated.len() {
                        let v = repeated.get(i);
                        singular_write_to(
                            &rtb,
                            &field_desc.get_proto().get_field_type(),
                            field_number,
                            &v,
                            os,
                        )?;
                    }
                }
                RuntimeFieldType::Map(_, _) => {
                    unimplemented!();
                }
            }
        }

        Ok(())
    }

    fn compute_size(&self) -> u32 {
        let mut m_size = 0;
        for field_desc in self.descriptor.fields() {
            let field_number = field_desc.get_proto().get_number() as u32;
            match field_desc.runtime_field_type() {
                RuntimeFieldType::Singular(rtb) => {
                    if let Some(v) = field_desc.get_singular(self) {
                        if v.is_non_zero() {
                            // ignore default value
                            m_size += compute_singular_size(
                                &rtb,
                                &field_desc.get_proto().get_field_type(),
                                field_number,
                                &v,
                            );
                        }
                    }
                }
                RuntimeFieldType::Repeated(rtb) => {
                    let repeated = field_desc.get_repeated(self);
                    if !repeated.is_empty() {
                        for i in 0..repeated.len() {
                            let v = repeated.get(i);
                            m_size += compute_singular_size(
                                &rtb,
                                &field_desc.get_proto().get_field_type(),
                                field_number,
                                &v,
                            );
                        }
                    }
                }
                RuntimeFieldType::Map(_, _) => {
                    unimplemented!();
                }
            }
        }
        // TODO: unknown fields
        m_size
    }

    fn get_cached_size(&self) -> u32 {
        self.cached_size.get()
    }

    fn get_unknown_fields(&self) -> &UnknownFields {
        &self.unknown_fields
    }

    fn mut_unknown_fields(&mut self) -> &mut UnknownFields {
        &mut self.unknown_fields
    }

    fn new() -> Self
    where
        Self: Sized,
    {
        panic!("DynamicMessage cannot be constructed directly")
    }

    fn default_instance() -> &'static Self
    where
        Self: Sized,
    {
        panic!("There's no default instance for dynamic message")
    }
}

/// Write singular field to output stream
fn singular_write_to(
    rtb: &RuntimeTypeBox,
    proto_type: &Type,
    field_number: u32,
    v: &ReflectValueRef,
    os: &mut CodedOutputStream,
) -> ProtobufResult<()> {
    match proto_type {
        Type::TYPE_ENUM => {
            assert!(matches!(rtb, RuntimeTypeBox::Enum(..)));
            if let RuntimeTypeBox::Enum(_) = rtb {
                let enum_v = v.to_enum_value().unwrap();
                os.write_enum(field_number, enum_v)?;
            } else {
                panic!("Protobuf type and Runtime type mismatch");
            }
        }
        Type::TYPE_MESSAGE => {
            assert!(matches!(rtb, RuntimeTypeBox::Message(..)));
            if let RuntimeTypeBox::Message(_) = rtb {
                let msg_v = v.to_message().unwrap();
                os.write_message_dyn(field_number, &*msg_v)?;
            } else {
                panic!("Protobuf type and Runtime type mismatch");
            }
        }
        Type::TYPE_GROUP => {
            unimplemented!()
        }
        Type::TYPE_UINT32 => {
            os.write_uint32(field_number, v.to_u32().unwrap())?;
        }
        Type::TYPE_UINT64 => {
            os.write_uint64(field_number, v.to_u64().unwrap())?;
        }
        Type::TYPE_INT32 => {
            os.write_int32(field_number, v.to_i32().unwrap())?;
        }
        Type::TYPE_INT64 => {
            os.write_int64(field_number, v.to_i64().unwrap())?;
        }
        Type::TYPE_SINT32 => {
            os.write_sint32(field_number, v.to_i32().unwrap())?;
        }
        Type::TYPE_SINT64 => {
            os.write_sint64(field_number, v.to_i64().unwrap())?;
        }
        Type::TYPE_FIXED32 => {
            os.write_fixed32(field_number, v.to_u32().unwrap())?;
        }
        Type::TYPE_FIXED64 => {
            os.write_fixed64(field_number, v.to_u64().unwrap())?;
        }
        Type::TYPE_SFIXED64 => {
            os.write_sfixed64(field_number, v.to_i64().unwrap())?;
        }
        Type::TYPE_SFIXED32 => {
            os.write_sfixed32(field_number, v.to_i32().unwrap())?;
        }
        Type::TYPE_BOOL => {
            os.write_bool(field_number, v.to_bool().unwrap())?;
        }
        Type::TYPE_STRING => {
            os.write_string(field_number, v.to_str().unwrap())?;
        }
        Type::TYPE_BYTES => {
            os.write_bytes(field_number, v.to_bytes().unwrap())?;
        }
        Type::TYPE_FLOAT => {
            os.write_float(field_number, v.to_f32().unwrap())?;
        }
        Type::TYPE_DOUBLE => {
            os.write_double(field_number, v.to_f64().unwrap())?;
        }
    };
    Ok(())
}

/// Compute singular field size
fn compute_singular_size(
    rtb: &RuntimeTypeBox,
    proto_type: &Type,
    field_number: u32,
    v: &ReflectValueRef,
) -> u32 {
    match proto_type {
        Type::TYPE_ENUM => {
            assert!(matches!(rtb, RuntimeTypeBox::Enum(..)));
            if let RuntimeTypeBox::Enum(_) = rtb {
                let enum_v = v.to_enum_value().unwrap();
                // we don't have a ProtobufEnum here, so just use the raw value
                value_size(field_number, enum_v, WireType::WireTypeVarint)
            } else {
                panic!("Protobuf type and Runtime type mismatch");
            }
        }
        Type::TYPE_MESSAGE => {
            assert!(matches!(rtb, RuntimeTypeBox::Message(..)));
            if let RuntimeTypeBox::Message(_) = rtb {
                let msg_v = v.to_message().unwrap();
                let len = msg_v.compute_size_dyn();
                tag_size(field_number) + compute_raw_varint32_size(len) + len
            } else {
                panic!("Protobuf type and Runtime type mismatch");
            }
        }
        Type::TYPE_GROUP => {
            unimplemented!()
        }
        Type::TYPE_UINT32 => {
            let typed_v = v.to_u32().unwrap();
            value_size(field_number, typed_v, WireType::WireTypeVarint)
        }
        Type::TYPE_UINT64 => {
            let typed_v = v.to_u64().unwrap();
            value_size(field_number, typed_v, WireType::WireTypeVarint)
        }
        Type::TYPE_INT32 => {
            let typed_v = v.to_i32().unwrap();
            value_size(field_number, typed_v, WireType::WireTypeVarint)
        }
        Type::TYPE_INT64 => {
            let typed_v = v.to_i64().unwrap();
            value_size(field_number, typed_v, WireType::WireTypeVarint)
        }
        Type::TYPE_SINT32 => {
            let typed_v = v.to_i32().unwrap();
            value_varint_zigzag_size(field_number, typed_v)
        }
        Type::TYPE_SINT64 => {
            let typed_v = v.to_i64().unwrap();
            value_varint_zigzag_size(field_number, typed_v)
        }
        Type::TYPE_FIXED32 => tag_size(field_number) + 4,
        Type::TYPE_FIXED64 => tag_size(field_number) + 8,
        Type::TYPE_SFIXED32 => tag_size(field_number) + 4,
        Type::TYPE_SFIXED64 => tag_size(field_number) + 8,
        Type::TYPE_BOOL => {
            let typed_v = v.to_bool().unwrap();
            value_size(field_number, typed_v, WireType::WireTypeVarint)
        }
        Type::TYPE_STRING => {
            let typed_v = v.to_str().unwrap();
            string_size(field_number, typed_v)
        }
        Type::TYPE_BYTES => {
            let typed_v = v.to_bytes().unwrap();
            bytes_size(field_number, typed_v)
        }
        Type::TYPE_FLOAT => tag_size(field_number) + 4,
        Type::TYPE_DOUBLE => tag_size(field_number) + 8,
    }
}
