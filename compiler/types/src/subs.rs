#![deny(unsafe_op_in_unsafe_fn)]
use crate::types::{
    name_type_var, AliasKind, ErrorType, Problem, RecordField, RecordFieldsError, TypeExt,
};
use roc_collections::all::{ImMap, ImSet, MutSet, SendMap};
use roc_error_macros::internal_error;
use roc_module::ident::{Lowercase, TagName, Uppercase};
use roc_module::symbol::Symbol;
use std::fmt;
use std::iter::{once, Iterator, Map};
use ven_ena::unify::{InPlace, Snapshot, UnificationTable, UnifyKey};

// if your changes cause this number to go down, great!
// please change it to the lower number.
// if it went up, maybe check that the change is really required
roc_error_macros::assert_sizeof_all!(Descriptor, 5 * 8);
roc_error_macros::assert_sizeof_all!(Content, 3 * 8 + 4);
roc_error_macros::assert_sizeof_all!(FlatType, 3 * 8);
roc_error_macros::assert_sizeof_all!(UnionTags, 12);
roc_error_macros::assert_sizeof_all!(RecordFields, 2 * 8);

roc_error_macros::assert_sizeof_aarch64!(Problem, 6 * 8);
roc_error_macros::assert_sizeof_wasm!(Problem, 32);
roc_error_macros::assert_sizeof_default!(Problem, 6 * 8);

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub struct Mark(i32);

impl Mark {
    pub const NONE: Mark = Mark(2);
    pub const OCCURS: Mark = Mark(1);
    pub const GET_VAR_NAMES: Mark = Mark(0);

    #[inline(always)]
    pub fn next(self) -> Mark {
        Mark(self.0 + 1)
    }
}

impl fmt::Debug for Mark {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self == &Mark::NONE {
            write!(f, "none")
        } else if self == &Mark::OCCURS {
            write!(f, "occurs")
        } else if self == &Mark::GET_VAR_NAMES {
            write!(f, "get_var_names")
        } else {
            write!(f, "Mark({})", self.0)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorTypeContext {
    None,
    ExpandRanges,
}

struct ErrorTypeState {
    taken: MutSet<Lowercase>,
    letters_used: u32,
    problems: Vec<crate::types::Problem>,
    context: ErrorTypeContext,
    recursive_tag_unions_seen: Vec<Variable>,
}

#[derive(Clone, Copy, Debug)]
struct SubsHeader {
    utable: u64,
    variables: u64,
    tag_names: u64,
    field_names: u64,
    record_fields: u64,
    variable_slices: u64,
    exposed_vars_by_symbol: u64,
}

impl SubsHeader {
    fn from_subs(subs: &Subs, exposed_vars_by_symbol: usize) -> Self {
        // TODO what do we do with problems? they should
        // be reported and then removed from Subs I think
        debug_assert!(subs.problems.is_empty());

        Self {
            utable: subs.utable.len() as u64,
            variables: subs.variables.len() as u64,
            tag_names: subs.tag_names.len() as u64,
            field_names: subs.field_names.len() as u64,
            record_fields: subs.record_fields.len() as u64,
            variable_slices: subs.variable_slices.len() as u64,
            exposed_vars_by_symbol: exposed_vars_by_symbol as u64,
        }
    }

    fn to_array(self) -> [u8; std::mem::size_of::<Self>()] {
        unsafe { std::mem::transmute(self) }
    }

    fn from_array(array: [u8; std::mem::size_of::<Self>()]) -> Self {
        unsafe { std::mem::transmute(array) }
    }
}

unsafe fn slice_as_bytes<T>(slice: &[T]) -> &[u8] {
    let ptr = slice.as_ptr();
    let byte_length = std::mem::size_of::<T>() * slice.len();

    unsafe { std::slice::from_raw_parts(ptr as *const u8, byte_length) }
}

fn round_to_multiple_of(value: usize, base: usize) -> usize {
    (value + (base - 1)) / base * base
}

enum SerializedTagName {
    Global(SubsSlice<u8>),
    Closure(Symbol),
}

impl Subs {
    pub fn serialize(
        &self,
        exposed_vars_by_symbol: &[(Symbol, Variable)],
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<usize> {
        let mut written = 0;

        let header = SubsHeader::from_subs(self, exposed_vars_by_symbol.len()).to_array();
        written += header.len();
        writer.write_all(&header)?;

        written = Self::serialize_unification_table(&self.utable, writer, written)?;

        written = Self::serialize_slice(&self.variables, writer, written)?;
        written = Self::serialize_tag_names(&self.tag_names, writer, written)?;
        written = Self::serialize_field_names(&self.field_names, writer, written)?;
        written = Self::serialize_slice(&self.record_fields, writer, written)?;
        written = Self::serialize_slice(&self.variable_slices, writer, written)?;
        written = Self::serialize_slice(exposed_vars_by_symbol, writer, written)?;

        Ok(written)
    }

    fn serialize_unification_table(
        utable: &UnificationTable<InPlace<Variable>>,
        writer: &mut impl std::io::Write,
        mut written: usize,
    ) -> std::io::Result<usize> {
        for i in 0..utable.len() {
            let var = unsafe { Variable::from_index(i as u32) };

            let desc = if utable.is_redirect(var) {
                let root = utable.get_root_key_without_compacting(var);

                // our strategy for a redirect; rank is max, mark is max, copy stores the var
                Descriptor {
                    content: Content::Error,
                    rank: Rank(u32::MAX),
                    mark: Mark(i32::MAX),
                    copy: root.into(),
                }
            } else {
                utable.probe_value_without_compacting(var)
            };

            let bytes: [u8; std::mem::size_of::<Descriptor>()] =
                unsafe { std::mem::transmute(desc) };
            written += bytes.len();
            writer.write_all(&bytes)?;
        }

        Ok(written)
    }

    /// Lowercase can be heap-allocated
    fn serialize_field_names(
        lowercases: &[Lowercase],
        writer: &mut impl std::io::Write,
        written: usize,
    ) -> std::io::Result<usize> {
        let mut buf: Vec<u8> = Vec::new();
        let mut slices: Vec<SubsSlice<u8>> = Vec::new();

        for field_name in lowercases {
            let slice =
                SubsSlice::extend_new(&mut buf, field_name.as_str().as_bytes().iter().copied());
            slices.push(slice);
        }

        let written = Self::serialize_slice(&slices, writer, written)?;

        Self::serialize_slice(&buf, writer, written)
    }

    /// Global tag names can be heap-allocated
    fn serialize_tag_names(
        tag_names: &[TagName],
        writer: &mut impl std::io::Write,
        written: usize,
    ) -> std::io::Result<usize> {
        let mut buf: Vec<u8> = Vec::new();
        let mut slices: Vec<SerializedTagName> = Vec::new();

        for tag_name in tag_names {
            let serialized = match tag_name {
                TagName::Tag(uppercase) => {
                    let slice = SubsSlice::extend_new(
                        &mut buf,
                        uppercase.as_str().as_bytes().iter().copied(),
                    );
                    SerializedTagName::Global(slice)
                }
                TagName::Closure(symbol) => SerializedTagName::Closure(*symbol),
            };

            slices.push(serialized);
        }

        let written = Self::serialize_slice(&slices, writer, written)?;

        Self::serialize_slice(&buf, writer, written)
    }

    fn serialize_slice<T>(
        slice: &[T],
        writer: &mut impl std::io::Write,
        written: usize,
    ) -> std::io::Result<usize> {
        let alignment = std::mem::align_of::<T>();
        let padding_bytes = round_to_multiple_of(written, alignment) - written;

        for _ in 0..padding_bytes {
            writer.write_all(&[0])?;
        }

        let bytes_slice = unsafe { slice_as_bytes(slice) };
        writer.write_all(bytes_slice)?;

        Ok(written + padding_bytes + bytes_slice.len())
    }

    pub fn deserialize(bytes: &[u8]) -> (Self, &[(Symbol, Variable)]) {

        let mut offset = 0;
        let header_slice = &bytes[..std::mem::size_of::<SubsHeader>()];
        offset += header_slice.len();
        let header = SubsHeader::from_array(header_slice.try_into().unwrap());

        let (utable, offset) =
            Self::deserialize_unification_table(bytes, header.utable as usize, offset);

        let (variables, offset) = Self::deserialize_slice(bytes, header.variables as usize, offset);
        let (tag_names, offset) =
            Self::deserialize_tag_names(bytes, header.tag_names as usize, offset);
        let (field_names, offset) =
            Self::deserialize_field_names(bytes, header.field_names as usize, offset);
        let (record_fields, offset) =
            Self::deserialize_slice(bytes, header.record_fields as usize, offset);
        let (variable_slices, offset) =
            Self::deserialize_slice(bytes, header.variable_slices as usize, offset);
        let (exposed_vars_by_symbol, _) =
            Self::deserialize_slice(bytes, header.exposed_vars_by_symbol as usize, offset);

        (
            Self {
                utable,
                variables: variables.to_vec(),
                tag_names: tag_names.to_vec(),
                field_names,
                record_fields: record_fields.to_vec(),
                variable_slices: variable_slices.to_vec(),
                tag_name_cache: Default::default(),
                problems: Default::default(),
            },
            exposed_vars_by_symbol,
        )
    }

    fn deserialize_unification_table(
        bytes: &[u8],
        length: usize,
        offset: usize,
    ) -> (UnificationTable<InPlace<Variable>>, usize) {
        let alignment = std::mem::align_of::<Descriptor>();
        let size = std::mem::size_of::<Descriptor>();
        debug_assert_eq!(offset, round_to_multiple_of(offset, alignment));

        let mut utable = UnificationTable::default();
        utable.reserve(length);

        let byte_length = length * size;
        let byte_slice = &bytes[offset..][..byte_length];

        let slice =
            unsafe { std::slice::from_raw_parts(byte_slice.as_ptr() as *const Descriptor, length) };

        let mut roots = Vec::new();

        for desc in slice {
            let var = utable.new_key(*desc);

            if desc.rank == Rank(u32::MAX) && desc.mark == Mark(i32::MAX) {
                let root = desc.copy.into_variable().unwrap();

                roots.push((var, root));
            }
        }

        for (var, root) in roots {
            let desc = utable.probe_value_without_compacting(root);
            utable.unify_roots(var, root, desc)
        }

        (utable, offset + byte_length)
    }

    fn deserialize_field_names(
        bytes: &[u8],
        length: usize,
        offset: usize,
    ) -> (Vec<Lowercase>, usize) {
        let (slices, mut offset) = Self::deserialize_slice::<SubsSlice<u8>>(bytes, length, offset);

        let string_slice = &bytes[offset..];

        let mut lowercases = Vec::with_capacity(length);
        for subs_slice in slices {
            let bytes = &string_slice[subs_slice.indices()];
            offset += bytes.len();
            let string = unsafe { std::str::from_utf8_unchecked(bytes) };

            lowercases.push(string.into());
        }

        (lowercases, offset)
    }

    fn deserialize_tag_names(bytes: &[u8], length: usize, offset: usize) -> (Vec<TagName>, usize) {
        let (slices, mut offset) =
            Self::deserialize_slice::<SerializedTagName>(bytes, length, offset);

        let string_slice = &bytes[offset..];

        let mut tag_names = Vec::with_capacity(length);
        for serialized_tag_name in slices {
            let tag_name = match serialized_tag_name {
                SerializedTagName::Global(subs_slice) => {
                    let bytes = &string_slice[subs_slice.indices()];
                    offset += bytes.len();
                    let string = unsafe { std::str::from_utf8_unchecked(bytes) };

                    TagName::Tag(string.into())
                }
                SerializedTagName::Closure(symbol) => TagName::Closure(*symbol),
            };

            tag_names.push(tag_name);
        }

        (tag_names, offset)
    }

    fn deserialize_slice<T>(bytes: &[u8], length: usize, mut offset: usize) -> (&[T], usize) {
        let alignment = std::mem::align_of::<T>();
        let size = std::mem::size_of::<T>();

        offset = round_to_multiple_of(offset, alignment);

        let byte_length = length * size;
        let byte_slice = &bytes[offset..][..byte_length];

        let slice = unsafe { std::slice::from_raw_parts(byte_slice.as_ptr() as *const T, length) };

        (slice, offset + byte_length)
    }
}

#[derive(Clone)]
pub struct Subs {
    utable: UnificationTable<InPlace<Variable>>,
    pub variables: Vec<Variable>,
    pub tag_names: Vec<TagName>,
    pub field_names: Vec<Lowercase>,
    pub record_fields: Vec<RecordField<()>>,
    pub variable_slices: Vec<VariableSubsSlice>,
    pub tag_name_cache: TagNameCache,
    pub problems: Vec<Problem>,
}

#[derive(Debug, Clone, Default)]
pub struct TagNameCache {
    globals: Vec<Uppercase>,
    globals_slices: Vec<SubsSlice<TagName>>,
    /// Just closure tags
    symbols: Vec<Symbol>,
    symbols_slices: Vec<SubsSlice<TagName>>,
}

impl TagNameCache {
    pub fn get_mut(&mut self, tag_name: &TagName) -> Option<&mut SubsSlice<TagName>> {
        match tag_name {
            TagName::Tag(uppercase) => {
                // force into block
                match self.globals.iter().position(|u| u == uppercase) {
                    Some(index) => Some(&mut self.globals_slices[index]),
                    None => None,
                }
            }
            TagName::Closure(symbol) => match self.symbols.iter().position(|s| s == symbol) {
                Some(index) => Some(&mut self.symbols_slices[index]),
                None => None,
            },
        }
    }

    pub fn push(&mut self, tag_name: &TagName, slice: SubsSlice<TagName>) {
        match tag_name {
            TagName::Tag(uppercase) => {
                self.globals.push(uppercase.clone());
                self.globals_slices.push(slice);
            }
            TagName::Closure(symbol) => {
                self.symbols.push(*symbol);
                self.symbols_slices.push(slice);
            }
        }
    }
}

impl Default for Subs {
    fn default() -> Self {
        Subs::new()
    }
}

/// A slice into the Vec<T> of subs
///
/// The starting position is a u32 which should be plenty
/// We limit slices to u16::MAX = 65535 elements
pub struct SubsSlice<T> {
    pub start: u32,
    pub length: u16,
    _marker: std::marker::PhantomData<T>,
}

/// An index into the Vec<T> of subs
pub struct SubsIndex<T> {
    pub index: u32,
    _marker: std::marker::PhantomData<T>,
}

// make `subs[some_index]` work. The types/trait resolution make sure we get the
// element from the right vector

impl std::ops::Index<SubsIndex<Variable>> for Subs {
    type Output = Variable;

    fn index(&self, index: SubsIndex<Variable>) -> &Self::Output {
        &self.variables[index.index as usize]
    }
}

impl std::ops::IndexMut<SubsIndex<Variable>> for Subs {
    fn index_mut(&mut self, index: SubsIndex<Variable>) -> &mut Self::Output {
        &mut self.variables[index.index as usize]
    }
}

impl std::ops::Index<SubsIndex<Lowercase>> for Subs {
    type Output = Lowercase;

    fn index(&self, index: SubsIndex<Lowercase>) -> &Self::Output {
        &self.field_names[index.index as usize]
    }
}

impl std::ops::Index<SubsIndex<TagName>> for Subs {
    type Output = TagName;

    fn index(&self, index: SubsIndex<TagName>) -> &Self::Output {
        &self.tag_names[index.index as usize]
    }
}

impl std::ops::IndexMut<SubsIndex<TagName>> for Subs {
    fn index_mut(&mut self, index: SubsIndex<TagName>) -> &mut Self::Output {
        &mut self.tag_names[index.index as usize]
    }
}

impl std::ops::IndexMut<SubsIndex<Lowercase>> for Subs {
    fn index_mut(&mut self, index: SubsIndex<Lowercase>) -> &mut Self::Output {
        &mut self.field_names[index.index as usize]
    }
}

impl std::ops::Index<SubsIndex<RecordField<()>>> for Subs {
    type Output = RecordField<()>;

    fn index(&self, index: SubsIndex<RecordField<()>>) -> &Self::Output {
        &self.record_fields[index.index as usize]
    }
}

impl std::ops::IndexMut<SubsIndex<RecordField<()>>> for Subs {
    fn index_mut(&mut self, index: SubsIndex<RecordField<()>>) -> &mut Self::Output {
        &mut self.record_fields[index.index as usize]
    }
}

impl std::ops::Index<SubsIndex<VariableSubsSlice>> for Subs {
    type Output = VariableSubsSlice;

    fn index(&self, index: SubsIndex<VariableSubsSlice>) -> &Self::Output {
        &self.variable_slices[index.index as usize]
    }
}

impl std::ops::IndexMut<SubsIndex<VariableSubsSlice>> for Subs {
    fn index_mut(&mut self, index: SubsIndex<VariableSubsSlice>) -> &mut Self::Output {
        &mut self.variable_slices[index.index as usize]
    }
}

// custom debug

impl<T> std::fmt::Debug for SubsIndex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SubsIndex<{}>({})",
            std::any::type_name::<T>(),
            self.index
        )
    }
}

impl<T> std::fmt::Debug for SubsSlice<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SubsSlice {{ start: {}, length: {} }}",
            self.start, self.length
        )
    }
}

// derive of copy and clone does not play well with PhantomData

impl<T> Copy for SubsIndex<T> {}

impl<T> Clone for SubsIndex<T> {
    fn clone(&self) -> Self {
        Self {
            index: self.index,
            _marker: self._marker,
        }
    }
}

impl<T> Copy for SubsSlice<T> {}

impl<T> Clone for SubsSlice<T> {
    fn clone(&self) -> Self {
        Self {
            start: self.start,
            length: self.length,
            _marker: self._marker,
        }
    }
}

impl<T> Default for SubsSlice<T> {
    fn default() -> Self {
        Self {
            start: Default::default(),
            length: Default::default(),
            _marker: Default::default(),
        }
    }
}

impl<T> SubsSlice<T> {
    pub fn get_slice<'a>(&self, slice: &'a [T]) -> &'a [T] {
        &slice[self.indices()]
    }

    pub fn get_slice_mut<'a>(&self, slice: &'a mut [T]) -> &'a mut [T] {
        &mut slice[self.indices()]
    }

    #[inline(always)]
    pub fn indices(&self) -> std::ops::Range<usize> {
        self.start as usize..(self.start as usize + self.length as usize)
    }

    pub const fn len(&self) -> usize {
        self.length as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub const fn new(start: u32, length: u16) -> Self {
        Self {
            start,
            length,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn extend_new(vec: &mut Vec<T>, it: impl IntoIterator<Item = T>) -> Self {
        let start = vec.len();

        vec.extend(it);

        let end = vec.len();

        Self::new(start as u32, (end - start) as u16)
    }
}

impl SubsSlice<VariableSubsSlice> {
    pub fn reserve_variable_slices(subs: &mut Subs, length: usize) -> Self {
        let start = subs.variable_slices.len() as u32;

        subs.variable_slices.reserve(length);

        let value = VariableSubsSlice::default();
        for _ in 0..length {
            subs.variable_slices.push(value);
        }

        Self::new(start, length as u16)
    }
}

impl SubsSlice<TagName> {
    pub fn reserve_tag_names(subs: &mut Subs, length: usize) -> Self {
        let start = subs.tag_names.len() as u32;

        subs.tag_names
            .extend(std::iter::repeat(TagName::Tag(Uppercase::default())).take(length));

        Self::new(start, length as u16)
    }
}

impl<T> SubsIndex<T> {
    pub const fn new(start: u32) -> Self {
        Self {
            index: start,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn push_new(vector: &mut Vec<T>, value: T) -> Self {
        let index = Self::new(vector.len() as _);

        vector.push(value);

        index
    }

    pub const fn as_slice(self) -> SubsSlice<T> {
        SubsSlice::new(self.index, 1)
    }
}

impl<T> IntoIterator for SubsSlice<T> {
    type Item = SubsIndex<T>;

    #[allow(clippy::type_complexity)]
    type IntoIter = Map<std::ops::Range<u32>, fn(u32) -> Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        (self.start..(self.start + self.length as u32)).map(u32_to_index)
    }
}

fn u32_to_index<T>(i: u32) -> SubsIndex<T> {
    SubsIndex {
        index: i,
        _marker: std::marker::PhantomData,
    }
}

pub trait GetSubsSlice<T> {
    fn get_subs_slice(&self, subs_slice: SubsSlice<T>) -> &[T];
}

impl GetSubsSlice<Variable> for Subs {
    fn get_subs_slice(&self, subs_slice: SubsSlice<Variable>) -> &[Variable] {
        subs_slice.get_slice(&self.variables)
    }
}

impl GetSubsSlice<RecordField<()>> for Subs {
    fn get_subs_slice(&self, subs_slice: SubsSlice<RecordField<()>>) -> &[RecordField<()>] {
        subs_slice.get_slice(&self.record_fields)
    }
}

impl GetSubsSlice<Lowercase> for Subs {
    fn get_subs_slice(&self, subs_slice: SubsSlice<Lowercase>) -> &[Lowercase] {
        subs_slice.get_slice(&self.field_names)
    }
}

impl fmt::Debug for Subs {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f)?;
        for i in 0..self.len() {
            let var = Variable(i as u32);
            let desc = self.get_without_compacting(var);

            let root = self.get_root_key_without_compacting(var);

            if var == root {
                write!(f, "{} => ", i)?;

                subs_fmt_desc(&desc, self, f)?;
            } else {
                write!(f, "{} => <{:?}>", i, root)?;
            }

            writeln!(f)?;
        }

        Ok(())
    }
}

fn subs_fmt_desc(this: &Descriptor, subs: &Subs, f: &mut fmt::Formatter) -> fmt::Result {
    subs_fmt_content(&this.content, subs, f)?;

    write!(f, " r: {:?}", &this.rank)?;
    write!(f, " m: {:?}", &this.mark)?;
    write!(f, " c: {:?}", &this.copy)
}

pub struct SubsFmtContent<'a>(pub &'a Content, pub &'a Subs);

impl<'a> fmt::Debug for SubsFmtContent<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        subs_fmt_content(self.0, self.1, f)
    }
}

fn subs_fmt_content(this: &Content, subs: &Subs, f: &mut fmt::Formatter) -> fmt::Result {
    match this {
        Content::FlexVar(name) => write!(f, "Flex({:?})", name),
        Content::FlexAbleVar(name, symbol) => write!(f, "FlexAble({:?}, {:?})", name, symbol),
        Content::RigidVar(name) => write!(f, "Rigid({:?})", name),
        Content::RigidAbleVar(name, symbol) => write!(f, "RigidAble({:?}, {:?})", name, symbol),
        Content::RecursionVar {
            structure,
            opt_name,
        } => write!(f, "Recursion({:?}, {:?})", structure, opt_name),
        Content::Structure(flat_type) => subs_fmt_flat_type(flat_type, subs, f),
        Content::Alias(name, arguments, actual, kind) => {
            let slice = subs.get_subs_slice(arguments.all_variables());
            let wrap = match kind {
                AliasKind::Structural => "Alias",
                AliasKind::Opaque => "Opaque",
            };

            write!(
                f,
                "{}({:?}, {:?}, <{:?}>{:?})",
                wrap,
                name,
                slice,
                actual,
                SubsFmtContent(subs.get_content_without_compacting(*actual), subs)
            )
        }
        Content::RangedNumber(typ, range) => {
            let slice = subs.get_subs_slice(*range);
            write!(f, "RangedNumber({:?}, {:?})", typ, slice)
        }
        Content::Error => write!(f, "Error"),
    }
}

pub struct SubsFmtFlatType<'a>(pub &'a FlatType, pub &'a Subs);

impl<'a> fmt::Debug for SubsFmtFlatType<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        subs_fmt_flat_type(self.0, self.1, f)
    }
}

fn subs_fmt_flat_type(this: &FlatType, subs: &Subs, f: &mut fmt::Formatter) -> fmt::Result {
    match this {
        FlatType::Apply(name, arguments) => {
            let slice = subs.get_subs_slice(*arguments);

            write!(f, "Apply({:?}, {:?})", name, slice)
        }
        FlatType::Func(arguments, lambda_set, result) => {
            let slice = subs.get_subs_slice(*arguments);
            write!(f, "Func([")?;
            for var in slice {
                let content = subs.get_content_without_compacting(*var);
                write!(f, "<{:?}>{:?},", *var, SubsFmtContent(content, subs))?;
            }
            let result_content = subs.get_content_without_compacting(*result);
            write!(
                f,
                "], {:?}, <{:?}>{:?})",
                lambda_set,
                *result,
                SubsFmtContent(result_content, subs)
            )
        }
        FlatType::Record(fields, ext) => {
            write!(f, "{{ ")?;

            let (it, new_ext) = fields.sorted_iterator_and_ext(subs, *ext);
            for (name, content) in it {
                let separator = match content {
                    RecordField::Optional(_) => '?',
                    RecordField::Required(_) => ':',
                    RecordField::Demanded(_) => ':',
                };
                write!(f, "{:?} {} {:?}, ", name, separator, content)?;
            }

            write!(f, "}}<{:?}>", new_ext)
        }
        FlatType::TagUnion(tags, ext) => {
            write!(f, "[ ")?;

            let (it, new_ext) = tags.sorted_iterator_and_ext(subs, *ext);
            for (name, slice) in it {
                write!(f, "{:?} ", name)?;
                for var in slice {
                    write!(
                        f,
                        "<{:?}>{:?} ",
                        var,
                        SubsFmtContent(subs.get_content_without_compacting(*var), subs)
                    )?;
                }
                write!(f, ", ")?;
            }

            write!(f, "]<{:?}>", new_ext)
        }
        FlatType::FunctionOrTagUnion(tagname_index, symbol, ext) => {
            let tagname: &TagName = &subs[*tagname_index];

            write!(
                f,
                "FunctionOrTagUnion({:?}, {:?}, {:?})",
                tagname, symbol, ext
            )
        }
        FlatType::RecursiveTagUnion(rec, tags, ext) => {
            write!(f, "[ ")?;

            let (it, new_ext) = tags.sorted_iterator_and_ext(subs, *ext);
            for (name, slice) in it {
                write!(f, "{:?} {:?}, ", name, slice)?;
            }

            write!(f, "]<{:?}> as <{:?}>", new_ext, rec)
        }
        FlatType::Erroneous(e) => write!(f, "Erroneous({:?})", e),
        FlatType::EmptyRecord => write!(f, "EmptyRecord"),
        FlatType::EmptyTagUnion => write!(f, "EmptyTagUnion"),
    }
}

#[derive(Debug)]
pub struct VarStore {
    next: u32,
}

impl Default for VarStore {
    fn default() -> Self {
        VarStore::new(Variable::FIRST_USER_SPACE_VAR)
    }
}

impl VarStore {
    #[inline(always)]
    pub fn new(next_var: Variable) -> Self {
        debug_assert!(next_var.0 >= Variable::FIRST_USER_SPACE_VAR.0);

        VarStore { next: next_var.0 }
    }

    pub fn new_from_subs(subs: &Subs) -> Self {
        let next_var = (subs.utable.len()) as u32;
        debug_assert!(next_var >= Variable::FIRST_USER_SPACE_VAR.0);

        VarStore { next: next_var }
    }

    pub fn peek(&mut self) -> u32 {
        self.next
    }

    pub fn fresh(&mut self) -> Variable {
        // Increment the counter and return the value it had before it was incremented.
        let answer = self.next;

        self.next += 1;

        Variable(answer)
    }

    pub fn fresh_lambda_set(&mut self) -> LambdaSet {
        LambdaSet(self.fresh())
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct OptVariable(u32);

impl OptVariable {
    pub const NONE: OptVariable = OptVariable(Variable::NULL.0);

    pub const fn is_none(self) -> bool {
        self.0 == Self::NONE.0
    }

    pub const fn is_some(self) -> bool {
        self.0 != Self::NONE.0
    }

    pub const fn into_variable(self) -> Option<Variable> {
        if self.is_none() {
            None
        } else {
            Some(Variable(self.0))
        }
    }

    pub fn unwrap_or_else<F>(self, or_else: F) -> Variable
    where
        F: FnOnce() -> Variable,
    {
        if self.is_none() {
            or_else()
        } else {
            Variable(self.0)
        }
    }
}

impl fmt::Debug for OptVariable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        (*self).into_variable().fmt(f)
    }
}

impl From<OptVariable> for Option<Variable> {
    fn from(opt_var: OptVariable) -> Self {
        opt_var.into_variable()
    }
}

/// Marks whether a when expression is exhaustive using a variable.
#[derive(Clone, Copy, Debug)]
pub struct ExhaustiveMark(Variable);

impl ExhaustiveMark {
    pub fn new(var_store: &mut VarStore) -> Self {
        Self(var_store.fresh())
    }

    // NOTE: only ever use this if you *know* a pattern match is surely exhaustive!
    // Otherwise you will get unpleasant unification errors.
    pub fn known_exhaustive() -> Self {
        Self(Variable::EMPTY_TAG_UNION)
    }

    pub fn variable_for_introduction(&self) -> Variable {
        debug_assert!(
            self.0 != Variable::EMPTY_TAG_UNION,
            "Attempting to introduce known mark"
        );
        self.0
    }

    pub fn set_non_exhaustive(&self, subs: &mut Subs) {
        subs.set_content(self.0, Content::Error);
    }

    pub fn is_non_exhaustive(&self, subs: &Subs) -> bool {
        matches!(subs.get_content_without_compacting(self.0), Content::Error)
    }
}

/// Marks whether a when branch is redundant using a variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RedundantMark(Variable);

impl RedundantMark {
    pub fn new(var_store: &mut VarStore) -> Self {
        Self(var_store.fresh())
    }

    // NOTE: only ever use this if you *know* a pattern match is surely exhaustive!
    // Otherwise you will get unpleasant unification errors.
    pub fn known_non_redundant() -> Self {
        Self(Variable::EMPTY_TAG_UNION)
    }

    pub fn variable_for_introduction(&self) -> Variable {
        debug_assert!(
            self.0 != Variable::EMPTY_TAG_UNION,
            "Attempting to introduce known mark"
        );
        self.0
    }

    pub fn set_redundant(&self, subs: &mut Subs) {
        subs.set_content(self.0, Content::Error);
    }

    pub fn is_redundant(&self, subs: &Subs) -> bool {
        matches!(subs.get_content_without_compacting(self.0), Content::Error)
    }
}

pub fn new_marks(var_store: &mut VarStore) -> (RedundantMark, ExhaustiveMark) {
    (
        RedundantMark::new(var_store),
        ExhaustiveMark::new(var_store),
    )
}

/// Marks whether a recursive let-cycle was determined to be illegal during solving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IllegalCycleMark(OptVariable);

impl IllegalCycleMark {
    pub fn new(var_store: &mut VarStore) -> Self {
        Self(OptVariable(var_store.fresh().index()))
    }

    /// used for recursive blocks with just one function; invalid recursion in such blocks is
    /// always a type error, so we don't need to generate a custom error message in such cases
    pub const fn empty() -> Self {
        Self(OptVariable::NONE)
    }

    pub fn set_illegal(&self, subs: &mut Subs) {
        if let Some(var) = self.0.into_variable() {
            subs.set_content(var, Content::Error);
        }
    }

    pub fn is_illegal(&self, subs: &Subs) -> bool {
        if let Some(var) = self.0.into_variable() {
            matches!(subs.get_content_without_compacting(var), Content::Error)
        } else {
            false
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Variable(u32);

macro_rules! define_const_var {
    ($($(:pub)? $name:ident),* $(,)?) => {
        #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
        enum ConstVariables {
            $( $name, )*
            FINAL_CONST_VAR
        }

        impl Variable {
            $( pub const $name: Variable = Variable(ConstVariables::$name as u32); )*

            pub const NUM_RESERVED_VARS: usize = ConstVariables::FINAL_CONST_VAR as usize;
        }

    };
}

define_const_var! {
    // Reserved for indicating the absence of a variable.
    // This lets us avoid using Option<Variable> for the Descriptor's
    // copy field, which is a relevant space savings because we make
    // a *ton* of Descriptors.
    //
    // Also relevant: because this has the value 0, Descriptors can 0-initialize
    // to it in bulk - which is relevant, because Descriptors get initialized in bulk.
    NULL,

    :pub EMPTY_RECORD,
    :pub EMPTY_TAG_UNION,

    BOOL_ENUM,
    :pub BOOL,

    ORDER_ENUM,
    :pub ORDER,

    // Signed8 := []
    :pub SIGNED8,
    :pub SIGNED16,
    :pub SIGNED32,
    :pub SIGNED64,
    :pub SIGNED128,

    :pub UNSIGNED8,
    :pub UNSIGNED16,
    :pub UNSIGNED32,
    :pub UNSIGNED64,
    :pub UNSIGNED128,

    :pub NATURAL,

    // Integer Signed8 := Signed8
    INTEGER_SIGNED8,
    INTEGER_SIGNED16,
    INTEGER_SIGNED32,
    INTEGER_SIGNED64,
    INTEGER_SIGNED128,

    INTEGER_UNSIGNED8,
    INTEGER_UNSIGNED16,
    INTEGER_UNSIGNED32,
    INTEGER_UNSIGNED64,
    INTEGER_UNSIGNED128,

    INTEGER_NATURAL,

    // Num (Integer Signed8) := Integer Signed8
    NUM_INTEGER_SIGNED8,
    NUM_INTEGER_SIGNED16,
    NUM_INTEGER_SIGNED32,
    NUM_INTEGER_SIGNED64,
    NUM_INTEGER_SIGNED128,

    NUM_INTEGER_UNSIGNED8,
    NUM_INTEGER_UNSIGNED16,
    NUM_INTEGER_UNSIGNED32,
    NUM_INTEGER_UNSIGNED64,
    NUM_INTEGER_UNSIGNED128,

    NUM_INTEGER_NATURAL,

    // I8 : Num (Integer Signed8)
    :pub I8,
    :pub I16,
    :pub I32,
    :pub I64,
    :pub I128,

    :pub U8,
    :pub U16,
    :pub U32,
    :pub U64,
    :pub U128,

    :pub NAT,

    // Binary32 : []
    BINARY32,
    BINARY64,
    DECIMAL,

    // Float Binary32 := Binary32
    FLOAT_BINARY32,
    FLOAT_BINARY64,
    FLOAT_DECIMAL,

    // Num (Float Binary32) := Float Binary32
    NUM_FLOAT_BINARY32,
    NUM_FLOAT_BINARY64,
    NUM_FLOAT_DECIMAL,

    :pub F32,
    :pub F64,

    :pub DEC,
}

impl Variable {
    const FIRST_USER_SPACE_VAR: Variable = Variable(Self::NUM_RESERVED_VARS as u32);

    /// # Safety
    ///
    /// It is not guaranteed that the variable is in bounds.
    pub unsafe fn from_index(v: u32) -> Self {
        Variable(v)
    }

    pub const fn index(&self) -> u32 {
        self.0
    }

    pub const fn get_reserved(symbol: Symbol) -> Option<Variable> {
        // Must be careful here: the variables must in fact be in Subs
        match symbol {
            Symbol::NUM_I128 => Some(Variable::I128),
            Symbol::NUM_I64 => Some(Variable::I64),
            Symbol::NUM_I32 => Some(Variable::I32),
            Symbol::NUM_I16 => Some(Variable::I16),
            Symbol::NUM_I8 => Some(Variable::I8),

            Symbol::NUM_U128 => Some(Variable::U128),
            Symbol::NUM_U64 => Some(Variable::U64),
            Symbol::NUM_U32 => Some(Variable::U32),
            Symbol::NUM_U16 => Some(Variable::U16),
            Symbol::NUM_U8 => Some(Variable::U8),

            Symbol::NUM_NAT => Some(Variable::NAT),

            Symbol::BOOL_BOOL => Some(Variable::BOOL),

            Symbol::NUM_F64 => Some(Variable::F64),
            Symbol::NUM_F32 => Some(Variable::F32),

            Symbol::NUM_DEC => Some(Variable::DEC),

            _ => None,
        }
    }
}

impl From<Variable> for OptVariable {
    fn from(var: Variable) -> Self {
        OptVariable(var.0)
    }
}

impl fmt::Debug for Variable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl UnifyKey for Variable {
    type Value = Descriptor;

    fn index(&self) -> u32 {
        self.0
    }

    fn from_index(index: u32) -> Self {
        Variable(index)
    }

    fn tag() -> &'static str {
        "Variable"
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LambdaSet(pub Variable);

impl fmt::Debug for LambdaSet {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "LambdaSet({})", self.0 .0)
    }
}

impl LambdaSet {
    pub fn into_inner(self) -> Variable {
        self.0
    }

    pub fn as_inner(&self) -> &Variable {
        &self.0
    }
}

impl From<Variable> for LambdaSet {
    fn from(variable: Variable) -> Self {
        LambdaSet(variable)
    }
}

/// Used in SolvedType
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VarId(u32);

impl VarId {
    pub fn from_var(var: Variable, subs: &Subs) -> Self {
        let var = subs.get_root_key_without_compacting(var);
        let Variable(n) = var;

        VarId(n)
    }

    pub const fn from_u32(n: u32) -> Self {
        VarId(n)
    }
}

impl fmt::Debug for VarId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[allow(clippy::too_many_arguments)]
fn integer_type(
    subs: &mut Subs,

    num_signed64: Symbol,
    num_i64: Symbol,

    signed64: Variable,

    integer_signed64: Variable,

    num_integer_signed64: Variable,

    var_i64: Variable,
) {
    // define the type Signed64 := []
    {
        subs.set_content(signed64, {
            Content::Alias(
                num_signed64,
                AliasVariables::default(),
                Variable::EMPTY_TAG_UNION,
                AliasKind::Opaque,
            )
        });
    }

    // define the type `Num.Integer Num.Signed64 := Num.Signed64`
    {
        let vars = AliasVariables::insert_into_subs(subs, [signed64], []);
        subs.set_content(integer_signed64, {
            Content::Alias(Symbol::NUM_INTEGER, vars, signed64, AliasKind::Opaque)
        });
    }

    // define the type `Num.Num (Num.Integer Num.Signed64) := Num.Integer Num.Signed64`
    {
        let vars = AliasVariables::insert_into_subs(subs, [integer_signed64], []);
        subs.set_content(num_integer_signed64, {
            Content::Alias(Symbol::NUM_NUM, vars, integer_signed64, AliasKind::Opaque)
        });
    }

    // define the type `Num.I64 : Num.Num (Num.Integer Num.Signed64)`
    {
        subs.set_content(var_i64, {
            Content::Alias(
                num_i64,
                AliasVariables::default(),
                num_integer_signed64,
                AliasKind::Structural,
            )
        });
    }
}

fn define_integer_types(subs: &mut Subs) {
    integer_type(
        subs,
        Symbol::NUM_SIGNED128,
        Symbol::NUM_I128,
        Variable::SIGNED128,
        Variable::INTEGER_SIGNED128,
        Variable::NUM_INTEGER_SIGNED128,
        Variable::I128,
    );

    integer_type(
        subs,
        Symbol::NUM_SIGNED64,
        Symbol::NUM_I64,
        Variable::SIGNED64,
        Variable::INTEGER_SIGNED64,
        Variable::NUM_INTEGER_SIGNED64,
        Variable::I64,
    );

    integer_type(
        subs,
        Symbol::NUM_SIGNED32,
        Symbol::NUM_I32,
        Variable::SIGNED32,
        Variable::INTEGER_SIGNED32,
        Variable::NUM_INTEGER_SIGNED32,
        Variable::I32,
    );

    integer_type(
        subs,
        Symbol::NUM_SIGNED16,
        Symbol::NUM_I16,
        Variable::SIGNED16,
        Variable::INTEGER_SIGNED16,
        Variable::NUM_INTEGER_SIGNED16,
        Variable::I16,
    );

    integer_type(
        subs,
        Symbol::NUM_SIGNED8,
        Symbol::NUM_I8,
        Variable::SIGNED8,
        Variable::INTEGER_SIGNED8,
        Variable::NUM_INTEGER_SIGNED8,
        Variable::I8,
    );

    integer_type(
        subs,
        Symbol::NUM_UNSIGNED128,
        Symbol::NUM_U128,
        Variable::UNSIGNED128,
        Variable::INTEGER_UNSIGNED128,
        Variable::NUM_INTEGER_UNSIGNED128,
        Variable::U128,
    );

    integer_type(
        subs,
        Symbol::NUM_UNSIGNED64,
        Symbol::NUM_U64,
        Variable::UNSIGNED64,
        Variable::INTEGER_UNSIGNED64,
        Variable::NUM_INTEGER_UNSIGNED64,
        Variable::U64,
    );

    integer_type(
        subs,
        Symbol::NUM_UNSIGNED32,
        Symbol::NUM_U32,
        Variable::UNSIGNED32,
        Variable::INTEGER_UNSIGNED32,
        Variable::NUM_INTEGER_UNSIGNED32,
        Variable::U32,
    );

    integer_type(
        subs,
        Symbol::NUM_UNSIGNED16,
        Symbol::NUM_U16,
        Variable::UNSIGNED16,
        Variable::INTEGER_UNSIGNED16,
        Variable::NUM_INTEGER_UNSIGNED16,
        Variable::U16,
    );

    integer_type(
        subs,
        Symbol::NUM_UNSIGNED8,
        Symbol::NUM_U8,
        Variable::UNSIGNED8,
        Variable::INTEGER_UNSIGNED8,
        Variable::NUM_INTEGER_UNSIGNED8,
        Variable::U8,
    );

    integer_type(
        subs,
        Symbol::NUM_NATURAL,
        Symbol::NUM_NAT,
        Variable::NATURAL,
        Variable::INTEGER_NATURAL,
        Variable::NUM_INTEGER_NATURAL,
        Variable::NAT,
    );
}

#[allow(clippy::too_many_arguments)]
fn float_type(
    subs: &mut Subs,

    num_binary64: Symbol,
    num_f64: Symbol,

    binary64: Variable,

    float_binary64: Variable,

    num_float_binary64: Variable,

    var_f64: Variable,
) {
    // define the type Binary64 := []
    {
        subs.set_content(binary64, {
            Content::Alias(
                num_binary64,
                AliasVariables::default(),
                Variable::EMPTY_TAG_UNION,
                AliasKind::Structural,
            )
        });
    }

    // define the type `Num.Float Num.Binary64 := Num.Binary64`
    {
        let vars = AliasVariables::insert_into_subs(subs, [binary64], []);
        subs.set_content(float_binary64, {
            Content::Alias(Symbol::NUM_FLOATINGPOINT, vars, binary64, AliasKind::Opaque)
        });
    }

    // define the type `Num.Num (Num.Float Num.Binary64) := Num.Float Num.Binary64`
    {
        let vars = AliasVariables::insert_into_subs(subs, [float_binary64], []);
        subs.set_content(num_float_binary64, {
            Content::Alias(Symbol::NUM_NUM, vars, float_binary64, AliasKind::Opaque)
        });
    }

    // define the type `F64: Num.Num (Num.Float Num.Binary64)`
    {
        subs.set_content(var_f64, {
            Content::Alias(
                num_f64,
                AliasVariables::default(),
                num_float_binary64,
                AliasKind::Structural,
            )
        });
    }
}

fn define_float_types(subs: &mut Subs) {
    float_type(
        subs,
        Symbol::NUM_BINARY32,
        Symbol::NUM_F32,
        Variable::BINARY32,
        Variable::FLOAT_BINARY32,
        Variable::NUM_FLOAT_BINARY32,
        Variable::F32,
    );

    float_type(
        subs,
        Symbol::NUM_BINARY64,
        Symbol::NUM_F64,
        Variable::BINARY64,
        Variable::FLOAT_BINARY64,
        Variable::NUM_FLOAT_BINARY64,
        Variable::F64,
    );

    float_type(
        subs,
        Symbol::NUM_DECIMAL,
        Symbol::NUM_DEC,
        Variable::DECIMAL,
        Variable::FLOAT_DECIMAL,
        Variable::NUM_FLOAT_DECIMAL,
        Variable::DEC,
    );
}

impl Subs {
    pub const RESULT_TAG_NAMES: SubsSlice<TagName> = SubsSlice::new(0, 2);
    pub const TAG_NAME_ERR: SubsIndex<TagName> = SubsIndex::new(0);
    pub const TAG_NAME_OK: SubsIndex<TagName> = SubsIndex::new(1);
    pub const TAG_NAME_INVALID_NUM_STR: SubsIndex<TagName> = SubsIndex::new(2);
    pub const TAG_NAME_BAD_UTF_8: SubsIndex<TagName> = SubsIndex::new(3);
    pub const TAG_NAME_OUT_OF_BOUNDS: SubsIndex<TagName> = SubsIndex::new(4);

    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(Variable::NUM_RESERVED_VARS);

        let mut tag_names = Vec::with_capacity(32);

        tag_names.push(TagName::Tag("Err".into()));
        tag_names.push(TagName::Tag("Ok".into()));

        tag_names.push(TagName::Tag("InvalidNumStr".into()));
        tag_names.push(TagName::Tag("BadUtf8".into()));
        tag_names.push(TagName::Tag("OutOfBounds".into()));

        let mut subs = Subs {
            utable: UnificationTable::default(),
            variables: Vec::new(),
            tag_names,
            field_names: Vec::new(),
            record_fields: Vec::new(),
            // store an empty slice at the first position
            // used for "TagOrFunction"
            variable_slices: vec![VariableSubsSlice::default()],
            tag_name_cache: TagNameCache::default(),
            problems: Vec::new(),
        };

        // NOTE the utable does not (currently) have a with_capacity; using this as the next-best thing
        subs.utable.reserve(capacity);

        // TODO There are at least these opportunities for performance optimization here:
        // * Making the default flex_var_descriptor be all 0s, so no init step is needed.

        for _ in 0..capacity {
            subs.utable.new_key(flex_var_descriptor());
        }

        define_integer_types(&mut subs);
        define_float_types(&mut subs);

        subs.set_content(
            Variable::EMPTY_RECORD,
            Content::Structure(FlatType::EmptyRecord),
        );
        subs.set_content(
            Variable::EMPTY_TAG_UNION,
            Content::Structure(FlatType::EmptyTagUnion),
        );

        let bool_union_tags = UnionTags::insert_into_subs(
            &mut subs,
            [
                (TagName::Tag("False".into()), []),
                (TagName::Tag("True".into()), []),
            ],
        );

        subs.set_content(Variable::BOOL_ENUM, {
            Content::Structure(FlatType::TagUnion(
                bool_union_tags,
                Variable::EMPTY_TAG_UNION,
            ))
        });

        subs.set_content(Variable::BOOL, {
            Content::Alias(
                Symbol::BOOL_BOOL,
                AliasVariables::default(),
                Variable::BOOL_ENUM,
                AliasKind::Structural,
            )
        });

        subs
    }

    pub fn new_from_varstore(var_store: VarStore) -> Self {
        let entries = var_store.next;

        Self::with_capacity(entries as usize)
    }

    pub fn extend_by(&mut self, entries: usize) {
        self.utable.reserve(entries);
        for _ in 0..entries {
            self.utable.new_key(flex_var_descriptor());
        }
    }

    #[inline(always)]
    pub fn fresh(&mut self, value: Descriptor) -> Variable {
        self.utable.new_key(value)
    }

    #[inline(always)]
    pub fn fresh_unnamed_flex_var(&mut self) -> Variable {
        self.fresh(Descriptor::from(unnamed_flex_var()))
    }

    pub fn rigid_var(&mut self, var: Variable, name: Lowercase) {
        let name_index = SubsIndex::push_new(&mut self.field_names, name);
        let content = Content::RigidVar(name_index);
        let desc = Descriptor::from(content);

        self.set(var, desc);
    }

    pub fn rigid_able_var(&mut self, var: Variable, name: Lowercase, ability: Symbol) {
        let name_index = SubsIndex::push_new(&mut self.field_names, name);
        let content = Content::RigidAbleVar(name_index, ability);
        let desc = Descriptor::from(content);

        self.set(var, desc);
    }

    /// Unions two keys without the possibility of failure.
    pub fn union(&mut self, left: Variable, right: Variable, desc: Descriptor) {
        let l_root = self.utable.inlined_get_root_key(left);
        let r_root = self.utable.inlined_get_root_key(right);

        // NOTE this swapping is intentional! most of our unifying commands are based on the elm
        // source, but unify_roots is from `ena`, not the elm source. Turns out that they have
        // different ideas of how the merge should go (l into r or the reverse), and this matters!
        self.utable.unify_roots(r_root, l_root, desc)
    }

    pub fn get(&mut self, key: Variable) -> Descriptor {
        self.utable.probe_value(key)
    }

    pub fn get_ref(&self, key: Variable) -> &Descriptor {
        &self.utable.probe_value_ref(key).value
    }

    #[inline(always)]
    pub fn get_ref_mut(&mut self, key: Variable) -> &mut Descriptor {
        &mut self.utable.probe_value_ref_mut(key).value
    }

    pub fn get_rank(&self, key: Variable) -> Rank {
        self.utable.probe_value_ref(key).value.rank
    }

    pub fn get_mark(&self, key: Variable) -> Mark {
        self.utable.probe_value_ref(key).value.mark
    }

    pub fn get_rank_mark(&self, key: Variable) -> (Rank, Mark) {
        let desc = &self.utable.probe_value_ref(key).value;

        (desc.rank, desc.mark)
    }

    #[inline(always)]
    pub fn get_without_compacting(&self, key: Variable) -> Descriptor {
        self.utable.probe_value_without_compacting(key)
    }

    pub fn get_content_without_compacting(&self, key: Variable) -> &Content {
        &self.utable.probe_value_ref(key).value.content
    }

    #[inline(always)]
    pub fn get_root_key(&mut self, key: Variable) -> Variable {
        self.utable.inlined_get_root_key(key)
    }

    #[inline(always)]
    pub fn get_root_key_without_compacting(&self, key: Variable) -> Variable {
        self.utable.get_root_key_without_compacting(key)
    }

    #[inline(always)]
    pub fn set(&mut self, key: Variable, r_value: Descriptor) {
        let l_key = self.utable.inlined_get_root_key(key);

        self.utable.update_value(l_key, |node| node.value = r_value);
    }

    pub fn set_rank(&mut self, key: Variable, rank: Rank) {
        let l_key = self.utable.inlined_get_root_key(key);

        self.utable.update_value(l_key, |node| {
            node.value.rank = rank;
        });
    }

    pub fn set_mark(&mut self, key: Variable, mark: Mark) {
        let l_key = self.utable.inlined_get_root_key(key);

        self.utable.update_value(l_key, |node| {
            node.value.mark = mark;
        });
    }

    pub fn set_rank_mark(&mut self, key: Variable, rank: Rank, mark: Mark) {
        let l_key = self.utable.inlined_get_root_key(key);

        self.utable.update_value(l_key, |node| {
            node.value.rank = rank;
            node.value.mark = mark;
        });
    }

    pub fn set_content(&mut self, key: Variable, content: Content) {
        let l_key = self.utable.inlined_get_root_key(key);

        self.utable.update_value(l_key, |node| {
            node.value.content = content;
        });
    }

    pub fn modify<F>(&mut self, key: Variable, mapper: F)
    where
        F: Fn(&mut Descriptor),
    {
        mapper(self.get_ref_mut(key));
    }

    #[inline(always)]
    pub fn get_rank_set_mark(&mut self, key: Variable, mark: Mark) -> Rank {
        let l_key = self.utable.inlined_get_root_key(key);

        let mut rank = Rank::NONE;

        self.utable.update_value(l_key, |node| {
            node.value.mark = mark;
            rank = node.value.rank;
        });

        rank
    }

    pub fn equivalent(&mut self, left: Variable, right: Variable) -> bool {
        self.utable.unioned(left, right)
    }

    pub fn redundant(&self, var: Variable) -> bool {
        self.utable.is_redirect(var)
    }

    pub fn occurs(&self, var: Variable) -> Result<(), (Variable, Vec<Variable>)> {
        occurs(self, &[], var, false)
    }

    pub fn occurs_including_recursion_vars(
        &self,
        var: Variable,
    ) -> Result<(), (Variable, Vec<Variable>)> {
        occurs(self, &[], var, true)
    }

    pub fn mark_tag_union_recursive(
        &mut self,
        recursive: Variable,
        tags: UnionTags,
        ext_var: Variable,
    ) {
        let description = self.get(recursive);

        let rec_var = self.fresh_unnamed_flex_var();
        self.set_rank(rec_var, description.rank);
        self.set_content(
            rec_var,
            Content::RecursionVar {
                opt_name: None,
                structure: recursive,
            },
        );

        let new_variable_slices = SubsSlice::reserve_variable_slices(self, tags.len());

        let it = new_variable_slices.indices().zip(tags.iter_all());
        for (variable_slice_index, (_, slice_index)) in it {
            let slice = self[slice_index];

            let new_variables = VariableSubsSlice::reserve_into_subs(self, slice.len());
            for (target_index, var_index) in new_variables.indices().zip(slice) {
                let var = self[var_index];
                self.variables[target_index] = self.explicit_substitute(recursive, rec_var, var);
            }

            self.variable_slices[variable_slice_index] = new_variables;
        }

        let new_ext_var = self.explicit_substitute(recursive, rec_var, ext_var);

        let new_tags = UnionTags::from_slices(tags.tag_names(), new_variable_slices);

        let flat_type = FlatType::RecursiveTagUnion(rec_var, new_tags, new_ext_var);

        self.set_content(recursive, Content::Structure(flat_type));
    }

    pub fn explicit_substitute(
        &mut self,
        from: Variable,
        to: Variable,
        in_var: Variable,
    ) -> Variable {
        let x = self.get_root_key(from);
        let y = self.get_root_key(to);
        let z = self.get_root_key(in_var);
        let mut seen = ImSet::default();
        explicit_substitute(self, x, y, z, &mut seen)
    }

    pub fn var_to_error_type(&mut self, var: Variable) -> (ErrorType, Vec<Problem>) {
        self.var_to_error_type_contextual(var, ErrorTypeContext::None)
    }

    pub fn var_to_error_type_contextual(
        &mut self,
        var: Variable,
        context: ErrorTypeContext,
    ) -> (ErrorType, Vec<Problem>) {
        let names = get_var_names(self, var, ImMap::default());
        let mut taken = MutSet::default();

        for (name, _) in names {
            taken.insert(name);
        }

        let mut state = ErrorTypeState {
            taken,
            letters_used: 0,
            problems: Vec::new(),
            context,
            recursive_tag_unions_seen: Vec::new(),
        };

        (var_to_err_type(self, &mut state, var), state.problems)
    }

    pub fn restore(&mut self, var: Variable) {
        restore_help(self, var)
    }

    pub fn len(&self) -> usize {
        self.utable.len()
    }

    pub fn is_empty(&self) -> bool {
        self.utable.is_empty()
    }

    pub fn contains(&self, var: Variable) -> bool {
        (var.index() as usize) < self.len()
    }

    pub fn snapshot(&mut self) -> Snapshot<InPlace<Variable>> {
        self.utable.snapshot()
    }

    pub fn rollback_to(&mut self, snapshot: Snapshot<InPlace<Variable>>) {
        self.utable.rollback_to(snapshot)
    }

    pub fn commit_snapshot(&mut self, snapshot: Snapshot<InPlace<Variable>>) {
        self.utable.commit(snapshot)
    }

    pub fn vars_since_snapshot(
        &mut self,
        snapshot: &Snapshot<InPlace<Variable>>,
    ) -> core::ops::Range<Variable> {
        self.utable.vars_since_snapshot(snapshot)
    }

    /// Checks whether the content of `var`, or any nested content, satisfies the `predicate`.
    pub fn var_contains_content<P>(&self, var: Variable, predicate: P) -> bool
    where
        P: Fn(&Content) -> bool + Copy,
    {
        let mut seen_recursion_vars = MutSet::default();
        var_contains_content_help(self, var, predicate, &mut seen_recursion_vars)
    }
}

#[inline(always)]
fn flex_var_descriptor() -> Descriptor {
    Descriptor::from(unnamed_flex_var())
}

#[inline(always)]
const fn unnamed_flex_var() -> Content {
    Content::FlexVar(None)
}

#[derive(Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rank(u32);

impl Rank {
    pub const NONE: Rank = Rank(0);

    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }

    pub const fn toplevel() -> Self {
        Rank(1)
    }

    /// the rank at which we introduce imports.
    ///
    /// Type checking starts at rank 1 aka toplevel. When there are rigid/flex variables introduced by a
    /// constraint, then these must be generalized relative to toplevel, and hence are introduced at
    /// rank 2.
    ///
    /// We always use: even if there are no rigids imported, introducing at rank 2 is correct
    /// (if slightly inefficient) because there are no rigids anyway so generalization is trivial
    pub const fn import() -> Self {
        Rank(2)
    }

    pub fn next(self) -> Self {
        Rank(self.0 + 1)
    }

    pub fn into_usize(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for Rank {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<Rank> for usize {
    fn from(rank: Rank) -> Self {
        rank.0 as usize
    }
}

impl From<usize> for Rank {
    fn from(index: usize) -> Self {
        Rank(index as u32)
    }
}

#[derive(Clone, Copy)]
pub struct Descriptor {
    pub content: Content,
    pub rank: Rank,
    pub mark: Mark,
    pub copy: OptVariable,
}

impl fmt::Debug for Descriptor {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:?}, r: {:?}, m: {:?} c: {:?}",
            self.content,
            self.rank,
            self.mark,
            self.copy.into_variable()
        )
    }
}

impl Default for Descriptor {
    fn default() -> Self {
        unnamed_flex_var().into()
    }
}

impl From<Content> for Descriptor {
    fn from(content: Content) -> Descriptor {
        Descriptor {
            content,
            rank: Rank::NONE,
            mark: Mark::NONE,
            copy: OptVariable::NONE,
        }
    }
}

roc_error_macros::assert_sizeof_all!(Content, 3 * 8 + 4);
roc_error_macros::assert_sizeof_all!((Symbol, AliasVariables, Variable), 2 * 8 + 4);
roc_error_macros::assert_sizeof_all!(AliasVariables, 8);
roc_error_macros::assert_sizeof_all!(FlatType, 3 * 8);

roc_error_macros::assert_sizeof_aarch64!((Variable, Option<Lowercase>), 4 * 8);
roc_error_macros::assert_sizeof_wasm!((Variable, Option<Lowercase>), 4 * 4);
roc_error_macros::assert_sizeof_default!((Variable, Option<Lowercase>), 4 * 8);

roc_error_macros::assert_copyable!(Content);
roc_error_macros::assert_copyable!(Descriptor);

#[derive(Clone, Copy, Debug)]
pub enum Content {
    /// A type variable which the user did not name in an annotation,
    ///
    /// When we auto-generate a type var name, e.g. the "a" in (a -> a), we
    /// change the Option in here from None to Some.
    FlexVar(Option<SubsIndex<Lowercase>>),
    /// name given in a user-written annotation
    RigidVar(SubsIndex<Lowercase>),
    /// Like a [Self::FlexVar], but is also bound to an ability.
    /// This can only happen when unified with a [Self::RigidAbleVar].
    FlexAbleVar(Option<SubsIndex<Lowercase>>, Symbol),
    /// Like a [Self::RigidVar], but is also bound to an ability.
    /// For example, "a has Hash".
    RigidAbleVar(SubsIndex<Lowercase>, Symbol),
    /// name given to a recursion variable
    RecursionVar {
        structure: Variable,
        opt_name: Option<SubsIndex<Lowercase>>,
    },
    Structure(FlatType),
    Alias(Symbol, AliasVariables, Variable, AliasKind),
    RangedNumber(Variable, VariableSubsSlice),
    Error,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AliasVariables {
    pub variables_start: u32,
    pub all_variables_len: u16,

    /// an alias has type variables and lambda set variables
    pub type_variables_len: u16,
}

impl AliasVariables {
    pub const fn all_variables(&self) -> VariableSubsSlice {
        SubsSlice::new(self.variables_start, self.all_variables_len)
    }

    pub const fn type_variables(&self) -> VariableSubsSlice {
        SubsSlice::new(self.variables_start, self.type_variables_len)
    }

    pub const fn lambda_set_variables(&self) -> VariableSubsSlice {
        SubsSlice::new(
            self.variables_start + self.type_variables_len as u32,
            self.all_variables_len - self.type_variables_len,
        )
    }

    pub const fn len(&self) -> usize {
        self.type_variables_len as usize
    }

    pub const fn is_empty(&self) -> bool {
        self.type_variables_len == 0
    }

    pub fn replace_variables(
        &mut self,
        subs: &mut Subs,
        variables: impl IntoIterator<Item = Variable>,
    ) {
        let variables_start = subs.variables.len() as u32;
        subs.variables.extend(variables);
        let variables_len = (subs.variables.len() - variables_start as usize) as u16;

        debug_assert_eq!(variables_len, self.all_variables_len);

        self.variables_start = variables_start;
    }

    pub fn named_type_arguments(&self) -> impl Iterator<Item = SubsIndex<Variable>> {
        self.all_variables()
            .into_iter()
            .take(self.type_variables_len as usize)
    }

    pub fn unnamed_type_arguments(&self) -> impl Iterator<Item = SubsIndex<Variable>> {
        self.all_variables()
            .into_iter()
            .skip(self.type_variables_len as usize)
    }

    pub fn insert_into_subs<I1, I2>(
        subs: &mut Subs,
        type_arguments: I1,
        unnamed_arguments: I2,
    ) -> Self
    where
        I1: IntoIterator<Item = Variable>,
        I2: IntoIterator<Item = Variable>,
    {
        let variables_start = subs.variables.len() as u32;

        subs.variables.extend(type_arguments);

        let type_variables_len = (subs.variables.len() as u32 - variables_start) as u16;

        subs.variables.extend(unnamed_arguments);

        let all_variables_len = (subs.variables.len() as u32 - variables_start) as u16;

        if type_variables_len == 3 {
            panic!();
        }

        Self {
            variables_start,
            type_variables_len,
            all_variables_len,
        }
    }
}

impl IntoIterator for AliasVariables {
    type Item = <VariableSubsSlice as IntoIterator>::Item;

    type IntoIter = <VariableSubsSlice as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.all_variables().into_iter()
    }
}

impl Content {
    #[inline(always)]
    pub fn is_number(&self) -> bool {
        matches!(
            &self,
            Content::Structure(FlatType::Apply(Symbol::NUM_NUM, _))
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub enum FlatType {
    Apply(Symbol, VariableSubsSlice),
    Func(VariableSubsSlice, Variable, Variable),
    Record(RecordFields, Variable),
    TagUnion(UnionTags, Variable),
    FunctionOrTagUnion(SubsIndex<TagName>, Symbol, Variable),
    RecursiveTagUnion(Variable, UnionTags, Variable),
    Erroneous(SubsIndex<Problem>),
    EmptyRecord,
    EmptyTagUnion,
}

impl FlatType {
    pub fn get_singleton_tag_union<'a>(&'a self, subs: &'a Subs) -> Option<&'a TagName> {
        match self {
            Self::TagUnion(tags, ext) => {
                let tags = tags.unsorted_tags_and_ext(subs, *ext).0.tags;
                if tags.len() != 1 {
                    return None;
                }
                let (tag_name, vars) = tags[0];
                if !vars.is_empty() {
                    return None;
                }
                Some(tag_name)
            }
            _ => None,
        }
    }
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum Builtin {
    Str,
    Int,
    Float,
    EmptyRecord,
}

pub type VariableSubsSlice = SubsSlice<Variable>;

impl VariableSubsSlice {
    /// Reserve space for `length` variables in the subs.variables array
    ///
    /// This is useful when we know how many variables e.g. a loop will produce,
    /// but the loop itself also produces new variables. We often want to work
    /// with slices, and the loop itself would break up our contiguous slice of variables
    ///
    /// This function often helps prevent an intermediate array. See also `indices` above
    /// to conveniently get a slice or iterator over the indices
    pub fn reserve_into_subs(subs: &mut Subs, length: usize) -> Self {
        let start = subs.variables.len() as u32;

        subs.variables
            .extend(std::iter::repeat(Variable::NULL).take(length));

        Self::new(start, length as u16)
    }

    pub fn insert_into_subs<I>(subs: &mut Subs, input: I) -> Self
    where
        I: IntoIterator<Item = Variable>,
    {
        let start = subs.variables.len() as u32;

        subs.variables.extend(input.into_iter());

        let length = (subs.variables.len() as u32 - start) as u16;

        Self::new(start, length)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UnionTags {
    length: u16,
    tag_names_start: u32,
    variables_start: u32,
}

impl UnionTags {
    pub fn is_newtype_wrapper(&self, subs: &Subs) -> bool {
        if self.length != 1 {
            return false;
        }

        let slice = subs.variable_slices[self.variables_start as usize];
        slice.length == 1
    }

    pub fn is_newtype_wrapper_of_tag(&self, subs: &Subs) -> bool {
        self.is_newtype_wrapper(subs) && {
            let tags = &subs.tag_names[self.tag_names().indices()];
            matches!(tags[0], TagName::Tag(_))
        }
    }

    pub fn from_tag_name_index(index: SubsIndex<TagName>) -> Self {
        Self::from_slices(
            SubsSlice::new(index.index, 1),
            SubsSlice::new(0, 1), // the first variablesubsslice is the empty slice
        )
    }

    pub fn from_slices(
        tag_names: SubsSlice<TagName>,
        variables: SubsSlice<VariableSubsSlice>,
    ) -> Self {
        debug_assert_eq!(
            tag_names.len(),
            variables.len(),
            "tag name len != variables len: {:?} {:?}",
            tag_names,
            variables,
        );

        Self {
            length: tag_names.len() as u16,
            tag_names_start: tag_names.start,
            variables_start: variables.start,
        }
    }

    pub const fn tag_names(&self) -> SubsSlice<TagName> {
        SubsSlice::new(self.tag_names_start, self.length)
    }

    pub const fn variables(&self) -> SubsSlice<VariableSubsSlice> {
        SubsSlice::new(self.variables_start, self.length)
    }

    pub const fn len(&self) -> usize {
        self.length as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn compare<T>(x: &(TagName, T), y: &(TagName, T)) -> std::cmp::Ordering {
        first(x, y)
    }
    pub fn insert_into_subs<I, I2>(subs: &mut Subs, input: I) -> Self
    where
        I: IntoIterator<Item = (TagName, I2)>,
        I2: IntoIterator<Item = Variable>,
    {
        let tag_names_start = subs.tag_names.len() as u32;
        let variables_start = subs.variable_slices.len() as u32;

        let it = input.into_iter();
        let size_hint = it.size_hint().0;

        subs.tag_names.reserve(size_hint);
        subs.variable_slices.reserve(size_hint);

        let mut length = 0;
        for (k, v) in it {
            let variables = VariableSubsSlice::insert_into_subs(subs, v.into_iter());

            subs.tag_names.push(k);
            subs.variable_slices.push(variables);

            length += 1;
        }

        Self::from_slices(
            SubsSlice::new(tag_names_start, length),
            SubsSlice::new(variables_start, length),
        )
    }

    pub fn tag_without_arguments(subs: &mut Subs, tag_name: TagName) -> Self {
        subs.tag_names.push(tag_name);

        Self {
            length: 1,
            tag_names_start: (subs.tag_names.len() - 1) as u32,
            variables_start: 0,
        }
    }

    pub fn insert_slices_into_subs<I>(subs: &mut Subs, input: I) -> Self
    where
        I: IntoIterator<Item = (TagName, VariableSubsSlice)>,
    {
        let tag_names_start = subs.tag_names.len() as u32;
        let variables_start = subs.variable_slices.len() as u32;

        let it = input.into_iter();
        let size_hint = it.size_hint().0;

        subs.tag_names.reserve(size_hint);
        subs.variable_slices.reserve(size_hint);

        let mut length = 0;
        for (k, variables) in it {
            subs.tag_names.push(k);
            subs.variable_slices.push(variables);

            length += 1;
        }

        Self {
            length,
            tag_names_start,
            variables_start,
        }
    }

    pub fn iter_all(
        &self,
    ) -> impl Iterator<Item = (SubsIndex<TagName>, SubsIndex<VariableSubsSlice>)> + ExactSizeIterator
    {
        self.tag_names()
            .into_iter()
            .zip(self.variables().into_iter())
    }

    /// Iterator over (TagName, &[Variable]) pairs obtained by
    /// looking up slices in the given Subs
    pub fn iter_from_subs<'a>(
        &'a self,
        subs: &'a Subs,
    ) -> impl Iterator<Item = (&'a TagName, &'a [Variable])> + ExactSizeIterator {
        self.iter_all().map(move |(name_index, payload_index)| {
            (&subs[name_index], subs.get_subs_slice(subs[payload_index]))
        })
    }

    #[inline(always)]
    pub fn unsorted_iterator<'a>(
        &'a self,
        subs: &'a Subs,
        ext: Variable,
    ) -> impl Iterator<Item = (&TagName, &[Variable])> + 'a {
        let (it, _) = crate::types::gather_tags_unsorted_iter(subs, *self, ext);

        let f = move |(label, slice): (_, SubsSlice<Variable>)| (label, subs.get_subs_slice(slice));

        it.map(f)
    }

    #[inline(always)]
    pub fn unsorted_tags_and_ext<'a>(
        &'a self,
        subs: &'a Subs,
        ext: Variable,
    ) -> (UnsortedUnionTags<'a>, Variable) {
        let (it, ext) = crate::types::gather_tags_unsorted_iter(subs, *self, ext);
        let f = move |(label, slice): (_, SubsSlice<Variable>)| (label, subs.get_subs_slice(slice));
        let it = it.map(f);

        (UnsortedUnionTags { tags: it.collect() }, ext)
    }

    #[inline(always)]
    pub fn sorted_iterator_and_ext<'a>(
        &'_ self,
        subs: &'a Subs,
        ext: Variable,
    ) -> (SortedTagsIterator<'a>, Variable) {
        if is_empty_tag_union(subs, ext) {
            (
                Box::new(self.iter_all().map(move |(i1, i2)| {
                    let tag_name: &TagName = &subs[i1];
                    let subs_slice = subs[i2];

                    let slice = subs.get_subs_slice(subs_slice);

                    (tag_name.clone(), slice)
                })),
                ext,
            )
        } else {
            let union_structure = crate::types::gather_tags(subs, *self, ext);

            (
                Box::new(union_structure.fields.into_iter()),
                union_structure.ext,
            )
        }
    }

    #[inline(always)]
    pub fn sorted_slices_iterator_and_ext<'a>(
        &'_ self,
        subs: &'a Subs,
        ext: Variable,
    ) -> (SortedTagsSlicesIterator<'a>, Variable) {
        if is_empty_tag_union(subs, ext) {
            (
                Box::new(self.iter_all().map(move |(i1, i2)| {
                    let tag_name: &TagName = &subs[i1];
                    let subs_slice = subs[i2];

                    (tag_name.clone(), subs_slice)
                })),
                ext,
            )
        } else {
            let (fields, ext) = crate::types::gather_tags_slices(subs, *self, ext);

            (Box::new(fields.into_iter()), ext)
        }
    }
}

#[derive(Debug)]
pub struct UnsortedUnionTags<'a> {
    pub tags: Vec<(&'a TagName, &'a [Variable])>,
}

impl<'a> UnsortedUnionTags<'a> {
    pub fn is_newtype_wrapper(&self, _subs: &Subs) -> bool {
        if self.tags.len() != 1 {
            return false;
        }
        self.tags[0].1.len() == 1
    }

    pub fn get_newtype(&self, _subs: &Subs) -> (&TagName, Variable) {
        let (tag_name, vars) = self.tags[0];
        (tag_name, vars[0])
    }
}

pub type SortedTagsIterator<'a> = Box<dyn ExactSizeIterator<Item = (TagName, &'a [Variable])> + 'a>;
pub type SortedTagsSlicesIterator<'a> = Box<dyn Iterator<Item = (TagName, VariableSubsSlice)> + 'a>;

pub fn is_empty_tag_union(subs: &Subs, mut var: Variable) -> bool {
    use crate::subs::Content::*;
    use crate::subs::FlatType::*;

    loop {
        match subs.get_content_without_compacting(var) {
            FlexVar(_) => return true,
            Structure(EmptyTagUnion) => return true,
            Structure(TagUnion(sub_fields, sub_ext)) => {
                if !sub_fields.is_empty() {
                    return false;
                }

                var = *sub_ext;
            }
            Structure(RecursiveTagUnion(_, sub_fields, sub_ext)) => {
                if !sub_fields.is_empty() {
                    return false;
                }

                var = *sub_ext;
            }

            Alias(_, _, actual_var, _) => {
                // TODO according to elm/compiler: "TODO may be dropping useful alias info here"
                var = *actual_var;
            }

            _other => {
                return false;
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RecordFields {
    pub length: u16,
    pub field_names_start: u32,
    pub variables_start: u32,
    pub field_types_start: u32,
}

fn first<K: Ord, V>(x: &(K, V), y: &(K, V)) -> std::cmp::Ordering {
    x.0.cmp(&y.0)
}

pub type SortedIterator<'a> = Box<dyn Iterator<Item = (Lowercase, RecordField<Variable>)> + 'a>;

impl RecordFields {
    pub const fn len(&self) -> usize {
        self.length as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn empty() -> Self {
        Self {
            length: 0,
            field_names_start: 0,
            variables_start: 0,
            field_types_start: 0,
        }
    }

    pub const fn variables(&self) -> SubsSlice<Variable> {
        SubsSlice::new(self.variables_start, self.length)
    }

    pub const fn field_names(&self) -> SubsSlice<Lowercase> {
        SubsSlice::new(self.field_names_start, self.length)
    }

    pub const fn record_fields(&self) -> SubsSlice<RecordField<()>> {
        SubsSlice::new(self.field_types_start, self.length)
    }

    pub fn iter_variables(&self) -> impl Iterator<Item = SubsIndex<Variable>> {
        let slice = SubsSlice::new(self.variables_start, self.length);
        slice.into_iter()
    }

    pub fn has_only_optional_fields(&self, subs: &Subs) -> bool {
        let slice: SubsSlice<RecordField<()>> = SubsSlice::new(self.field_types_start, self.length);

        subs.get_subs_slice(slice)
            .iter()
            .all(|field| matches!(field, RecordField::Optional(_)))
    }

    pub fn compare(
        x: &(Lowercase, RecordField<Variable>),
        y: &(Lowercase, RecordField<Variable>),
    ) -> std::cmp::Ordering {
        first(x, y)
    }

    pub fn insert_into_subs<I>(subs: &mut Subs, input: I) -> Self
    where
        I: IntoIterator<Item = (Lowercase, RecordField<Variable>)>,
    {
        let field_names_start = subs.field_names.len() as u32;
        let variables_start = subs.variables.len() as u32;
        let field_types_start = subs.record_fields.len() as u32;

        let it = input.into_iter();
        let size_hint = it.size_hint().0;

        subs.variables.reserve(size_hint);
        subs.field_names.reserve(size_hint);
        subs.record_fields.reserve(size_hint);

        let mut length = 0;
        for (k, v) in it {
            let var = *v.as_inner();
            let record_field = v.map(|_| ());

            subs.field_names.push(k);
            subs.variables.push(var);
            subs.record_fields.push(record_field);

            length += 1;
        }

        RecordFields {
            length,
            field_names_start,
            variables_start,
            field_types_start,
        }
    }

    #[inline(always)]
    pub fn unsorted_iterator<'a>(
        &'a self,
        subs: &'a Subs,
        ext: Variable,
    ) -> Result<impl Iterator<Item = (&'a Lowercase, RecordField<Variable>)> + 'a, RecordFieldsError>
    {
        let (it, _) = crate::types::gather_fields_unsorted_iter(subs, *self, ext)?;

        Ok(it)
    }

    #[inline(always)]
    pub fn unsorted_iterator_and_ext<'a>(
        &'a self,
        subs: &'a Subs,
        ext: Variable,
    ) -> (
        impl Iterator<Item = (&Lowercase, RecordField<Variable>)> + 'a,
        Variable,
    ) {
        let (it, ext) = crate::types::gather_fields_unsorted_iter(subs, *self, ext)
            .expect("Something weird ended up in a record type");

        (it, ext)
    }

    /// Get a sorted iterator over the fields of this record type
    ///
    /// Implementation: When the record has an `ext` variable that is the empty record, then
    /// we read the (assumed sorted) fields directly from Subs. Otherwise we have to chase the
    /// ext var, then sort the fields.
    ///
    /// Hopefully the inline will get rid of the Box in practice
    #[inline(always)]
    pub fn sorted_iterator<'a>(&'_ self, subs: &'a Subs, ext: Variable) -> SortedIterator<'a> {
        self.sorted_iterator_and_ext(subs, ext).0
    }

    #[inline(always)]
    pub fn sorted_iterator_and_ext<'a>(
        &'_ self,
        subs: &'a Subs,
        ext: Variable,
    ) -> (SortedIterator<'a>, Variable) {
        if is_empty_record(subs, ext) {
            (
                Box::new(self.iter_all().map(move |(i1, i2, i3)| {
                    let field_name: Lowercase = subs[i1].clone();
                    let variable = subs[i2];
                    let record_field: RecordField<Variable> = subs[i3].map(|_| variable);

                    (field_name, record_field)
                })),
                ext,
            )
        } else {
            let record_structure = crate::types::gather_fields(subs, *self, ext)
                .expect("Something ended up weird in this record type");

            (
                Box::new(record_structure.fields.into_iter()),
                record_structure.ext,
            )
        }
    }

    pub fn iter_all(
        &self,
    ) -> impl Iterator<
        Item = (
            SubsIndex<Lowercase>,
            SubsIndex<Variable>,
            SubsIndex<RecordField<()>>,
        ),
    > {
        let helper = |start| start..(start + self.length as u32);

        let range1 = helper(self.field_names_start);
        let range2 = helper(self.variables_start);
        let range3 = helper(self.field_types_start);

        let it = range1
            .into_iter()
            .zip(range2.into_iter())
            .zip(range3.into_iter());

        it.map(|((i1, i2), i3)| (SubsIndex::new(i1), SubsIndex::new(i2), SubsIndex::new(i3)))
    }
}

fn is_empty_record(subs: &Subs, mut var: Variable) -> bool {
    use crate::subs::Content::*;
    use crate::subs::FlatType::*;

    loop {
        match subs.get_content_without_compacting(var) {
            Structure(EmptyRecord) => return true,
            Structure(Record(sub_fields, sub_ext)) => {
                if !sub_fields.is_empty() {
                    return false;
                }

                var = *sub_ext;
            }

            Alias(_, _, actual_var, _) => {
                // TODO according to elm/compiler: "TODO may be dropping useful alias info here"
                var = *actual_var;
            }

            _ => return false,
        }
    }
}

fn occurs(
    subs: &Subs,
    seen: &[Variable],
    input_var: Variable,
    include_recursion_var: bool,
) -> Result<(), (Variable, Vec<Variable>)> {
    use self::Content::*;
    use self::FlatType::*;

    let root_var = subs.get_root_key_without_compacting(input_var);

    if seen.contains(&root_var) {
        Err((root_var, vec![]))
    } else {
        match subs.get_content_without_compacting(root_var) {
            FlexVar(_)
            | RigidVar(_)
            | FlexAbleVar(_, _)
            | RigidAbleVar(_, _)
            | RecursionVar { .. }
            | Error => Ok(()),

            Structure(flat_type) => {
                let mut new_seen = seen.to_owned();

                new_seen.push(root_var);

                match flat_type {
                    Apply(_, args) => short_circuit(
                        subs,
                        root_var,
                        &new_seen,
                        subs.get_subs_slice(*args).iter(),
                        include_recursion_var,
                    ),
                    Func(arg_vars, closure_var, ret_var) => {
                        let it = once(ret_var)
                            .chain(once(closure_var))
                            .chain(subs.get_subs_slice(*arg_vars).iter());
                        short_circuit(subs, root_var, &new_seen, it, include_recursion_var)
                    }
                    Record(vars_by_field, ext_var) => {
                        let slice =
                            SubsSlice::new(vars_by_field.variables_start, vars_by_field.length);
                        let it = once(ext_var).chain(subs.get_subs_slice(slice).iter());
                        short_circuit(subs, root_var, &new_seen, it, include_recursion_var)
                    }
                    TagUnion(tags, ext_var) => {
                        for slice_index in tags.variables() {
                            let slice = subs[slice_index];
                            for var_index in slice {
                                let var = subs[var_index];
                                short_circuit_help(
                                    subs,
                                    root_var,
                                    &new_seen,
                                    var,
                                    include_recursion_var,
                                )?;
                            }
                        }

                        short_circuit_help(
                            subs,
                            root_var,
                            &new_seen,
                            *ext_var,
                            include_recursion_var,
                        )
                    }
                    FunctionOrTagUnion(_, _, ext_var) => {
                        let it = once(ext_var);
                        short_circuit(subs, root_var, &new_seen, it, include_recursion_var)
                    }
                    RecursiveTagUnion(rec_var, tags, ext_var) => {
                        if include_recursion_var {
                            new_seen.push(subs.get_root_key_without_compacting(*rec_var));
                        }
                        for slice_index in tags.variables() {
                            let slice = subs[slice_index];
                            for var_index in slice {
                                let var = subs[var_index];
                                short_circuit_help(
                                    subs,
                                    root_var,
                                    &new_seen,
                                    var,
                                    include_recursion_var,
                                )?;
                            }
                        }

                        short_circuit_help(
                            subs,
                            root_var,
                            &new_seen,
                            *ext_var,
                            include_recursion_var,
                        )
                    }
                    EmptyRecord | EmptyTagUnion | Erroneous(_) => Ok(()),
                }
            }
            Alias(_, args, _, _) => {
                let mut new_seen = seen.to_owned();
                new_seen.push(root_var);

                for var_index in args.into_iter() {
                    let var = subs[var_index];
                    short_circuit_help(subs, root_var, &new_seen, var, include_recursion_var)?;
                }

                Ok(())
            }
            RangedNumber(typ, _range_vars) => {
                let mut new_seen = seen.to_owned();
                new_seen.push(root_var);

                short_circuit_help(subs, root_var, &new_seen, *typ, include_recursion_var)?;
                // _range_vars excluded because they are not explicitly part of the type.

                Ok(())
            }
        }
    }
}

#[inline(always)]
fn short_circuit<'a, T>(
    subs: &Subs,
    root_key: Variable,
    seen: &[Variable],
    iter: T,
    include_recursion_var: bool,
) -> Result<(), (Variable, Vec<Variable>)>
where
    T: Iterator<Item = &'a Variable>,
{
    for var in iter {
        short_circuit_help(subs, root_key, seen, *var, include_recursion_var)?;
    }

    Ok(())
}

#[inline(always)]
fn short_circuit_help(
    subs: &Subs,
    root_key: Variable,
    seen: &[Variable],
    var: Variable,
    include_recursion_var: bool,
) -> Result<(), (Variable, Vec<Variable>)> {
    if let Err((v, mut vec)) = occurs(subs, seen, var, include_recursion_var) {
        vec.push(root_key);
        return Err((v, vec));
    }

    Ok(())
}

fn explicit_substitute(
    subs: &mut Subs,
    from: Variable,
    to: Variable,
    in_var: Variable,
    seen: &mut ImSet<Variable>,
) -> Variable {
    use self::Content::*;
    use self::FlatType::*;
    let in_root = subs.get_root_key(in_var);
    if seen.contains(&in_root) {
        in_var
    } else {
        seen.insert(in_root);

        if subs.get_root_key(from) == subs.get_root_key(in_var) {
            to
        } else {
            match subs.get(in_var).content {
                FlexVar(_)
                | RigidVar(_)
                | FlexAbleVar(_, _)
                | RigidAbleVar(_, _)
                | RecursionVar { .. }
                | Error => in_var,

                Structure(flat_type) => {
                    match flat_type {
                        Apply(symbol, args) => {
                            for var_index in args.into_iter() {
                                let var = subs[var_index];
                                let answer = explicit_substitute(subs, from, to, var, seen);
                                subs[var_index] = answer;
                            }

                            subs.set_content(in_var, Structure(Apply(symbol, args)));
                        }
                        Func(arg_vars, closure_var, ret_var) => {
                            for var_index in arg_vars.into_iter() {
                                let var = subs[var_index];
                                let answer = explicit_substitute(subs, from, to, var, seen);
                                subs[var_index] = answer;
                            }

                            let new_ret_var = explicit_substitute(subs, from, to, ret_var, seen);
                            let new_closure_var =
                                explicit_substitute(subs, from, to, closure_var, seen);

                            subs.set_content(
                                in_var,
                                Structure(Func(arg_vars, new_closure_var, new_ret_var)),
                            );
                        }
                        TagUnion(tags, ext_var) => {
                            let new_ext_var = explicit_substitute(subs, from, to, ext_var, seen);

                            let mut new_slices = Vec::new();
                            for slice_index in tags.variables() {
                                let slice = subs[slice_index];

                                let mut new_variables = Vec::new();
                                for var_index in slice {
                                    let var = subs[var_index];
                                    let new_var = explicit_substitute(subs, from, to, var, seen);
                                    new_variables.push(new_var);
                                }

                                let start = subs.variables.len() as u32;
                                let length = new_variables.len() as u16;

                                subs.variables.extend(new_variables);

                                new_slices.push(VariableSubsSlice::new(start, length));
                            }

                            let start = subs.variable_slices.len() as u32;
                            let length = new_slices.len();

                            subs.variable_slices.extend(new_slices);

                            let mut union_tags = tags;
                            debug_assert_eq!(length, union_tags.len());
                            union_tags.variables_start = start;

                            subs.set_content(in_var, Structure(TagUnion(union_tags, new_ext_var)));
                        }
                        FunctionOrTagUnion(tag_name, symbol, ext_var) => {
                            let new_ext_var = explicit_substitute(subs, from, to, ext_var, seen);
                            subs.set_content(
                                in_var,
                                Structure(FunctionOrTagUnion(tag_name, symbol, new_ext_var)),
                            );
                        }
                        RecursiveTagUnion(rec_var, tags, ext_var) => {
                            // NOTE rec_var is not substituted, verify that this is correct!
                            let new_ext_var = explicit_substitute(subs, from, to, ext_var, seen);

                            let mut new_slices = Vec::new();
                            for slice_index in tags.variables() {
                                let slice = subs[slice_index];

                                let mut new_variables = Vec::new();
                                for var_index in slice {
                                    let var = subs[var_index];
                                    let new_var = explicit_substitute(subs, from, to, var, seen);
                                    new_variables.push(new_var);
                                }

                                let start = subs.variables.len() as u32;
                                let length = new_variables.len() as u16;

                                subs.variables.extend(new_variables);

                                new_slices.push(VariableSubsSlice::new(start, length));
                            }

                            let start = subs.variable_slices.len() as u32;
                            let length = new_slices.len();

                            subs.variable_slices.extend(new_slices);

                            let mut union_tags = tags;
                            debug_assert_eq!(length, union_tags.len());
                            union_tags.variables_start = start;

                            subs.set_content(
                                in_var,
                                Structure(RecursiveTagUnion(rec_var, union_tags, new_ext_var)),
                            );
                        }
                        Record(vars_by_field, ext_var) => {
                            let new_ext_var = explicit_substitute(subs, from, to, ext_var, seen);

                            for index in vars_by_field.iter_variables() {
                                let var = subs[index];
                                let new_var = explicit_substitute(subs, from, to, var, seen);
                                subs[index] = new_var;
                            }

                            subs.set_content(in_var, Structure(Record(vars_by_field, new_ext_var)));
                        }

                        EmptyRecord | EmptyTagUnion | Erroneous(_) => {}
                    }

                    in_var
                }
                Alias(symbol, args, actual, kind) => {
                    for index in args.into_iter() {
                        let var = subs[index];
                        let new_var = explicit_substitute(subs, from, to, var, seen);
                        subs[index] = new_var;
                    }

                    let new_actual = explicit_substitute(subs, from, to, actual, seen);

                    subs.set_content(in_var, Alias(symbol, args, new_actual, kind));

                    in_var
                }
                RangedNumber(typ, vars) => {
                    for index in vars.into_iter() {
                        let var = subs[index];
                        let new_var = explicit_substitute(subs, from, to, var, seen);
                        subs[index] = new_var;
                    }

                    let new_typ = explicit_substitute(subs, from, to, typ, seen);

                    subs.set_content(in_var, RangedNumber(new_typ, vars));

                    in_var
                }
            }
        }
    }
}

fn get_var_names(
    subs: &mut Subs,
    var: Variable,
    taken_names: ImMap<Lowercase, Variable>,
) -> ImMap<Lowercase, Variable> {
    use self::Content::*;
    let desc = subs.get(var);

    if desc.mark == Mark::GET_VAR_NAMES {
        taken_names
    } else {
        subs.set_mark(var, Mark::GET_VAR_NAMES);

        match desc.content {
            Error | FlexVar(None) | FlexAbleVar(None, _) => taken_names,

            FlexVar(Some(name_index)) | FlexAbleVar(Some(name_index), _) => add_name(
                subs,
                0,
                name_index,
                var,
                |name| FlexVar(Some(name)),
                taken_names,
            ),

            RecursionVar {
                opt_name,
                structure,
            } => match opt_name {
                Some(name_index) => add_name(
                    subs,
                    0,
                    name_index,
                    var,
                    |name| RecursionVar {
                        opt_name: Some(name),
                        structure,
                    },
                    taken_names,
                ),
                None => taken_names,
            },

            RigidVar(name_index) | RigidAbleVar(name_index, _) => {
                add_name(subs, 0, name_index, var, RigidVar, taken_names)
            }

            Alias(_, args, _, _) => args.into_iter().fold(taken_names, |answer, arg_var| {
                get_var_names(subs, subs[arg_var], answer)
            }),

            RangedNumber(typ, vars) => {
                let taken_names = get_var_names(subs, typ, taken_names);
                vars.into_iter().fold(taken_names, |answer, var| {
                    get_var_names(subs, subs[var], answer)
                })
            }

            Structure(flat_type) => match flat_type {
                FlatType::Apply(_, args) => {
                    args.into_iter().fold(taken_names, |answer, arg_var| {
                        get_var_names(subs, subs[arg_var], answer)
                    })
                }

                FlatType::Func(arg_vars, closure_var, ret_var) => {
                    let taken_names = get_var_names(subs, ret_var, taken_names);
                    let taken_names = get_var_names(subs, closure_var, taken_names);

                    let mut accum = taken_names;

                    for var_index in arg_vars.into_iter() {
                        let arg_var = subs[var_index];

                        accum = get_var_names(subs, arg_var, accum)
                    }

                    accum
                }

                FlatType::EmptyRecord | FlatType::EmptyTagUnion | FlatType::Erroneous(_) => {
                    taken_names
                }

                FlatType::Record(vars_by_field, ext_var) => {
                    let mut accum = get_var_names(subs, ext_var, taken_names);

                    for var_index in vars_by_field.iter_variables() {
                        let arg_var = subs[var_index];

                        accum = get_var_names(subs, arg_var, accum)
                    }

                    accum
                }
                FlatType::TagUnion(tags, ext_var) => {
                    let mut taken_names = get_var_names(subs, ext_var, taken_names);

                    for slice_index in tags.variables() {
                        let slice = subs[slice_index];
                        for var_index in slice {
                            let var = subs[var_index];
                            taken_names = get_var_names(subs, var, taken_names)
                        }
                    }

                    taken_names
                }

                FlatType::FunctionOrTagUnion(_, _, ext_var) => {
                    get_var_names(subs, ext_var, taken_names)
                }

                FlatType::RecursiveTagUnion(rec_var, tags, ext_var) => {
                    let taken_names = get_var_names(subs, ext_var, taken_names);
                    let mut taken_names = get_var_names(subs, rec_var, taken_names);

                    for slice_index in tags.variables() {
                        let slice = subs[slice_index];
                        for var_index in slice {
                            let arg_var = subs[var_index];
                            taken_names = get_var_names(subs, arg_var, taken_names)
                        }
                    }

                    taken_names
                }
            },
        }
    }
}

fn add_name<F>(
    subs: &mut Subs,
    index: usize,
    given_name_index: SubsIndex<Lowercase>,
    var: Variable,
    content_from_name: F,
    taken_names: ImMap<Lowercase, Variable>,
) -> ImMap<Lowercase, Variable>
where
    F: FnOnce(SubsIndex<Lowercase>) -> Content,
{
    let given_name = subs.field_names[given_name_index.index as usize].clone();

    let indexed_name = if index == 0 {
        given_name.clone()
    } else {
        // TODO is this the proper use of index here, or should we be
        // doing something else like turning it into an ASCII letter?
        Lowercase::from(format!("{}{}", given_name, index))
    };

    match taken_names.get(&indexed_name) {
        None => {
            if indexed_name != given_name {
                let indexed_name_index =
                    SubsIndex::push_new(&mut subs.field_names, indexed_name.clone());
                subs.set_content(var, content_from_name(indexed_name_index));
            }

            let mut answer = taken_names.clone();

            answer.insert(indexed_name, var);

            taken_names
        }
        Some(&other_var) => {
            if subs.equivalent(var, other_var) {
                taken_names
            } else {
                add_name(
                    subs,
                    index + 1,
                    given_name_index,
                    var,
                    content_from_name,
                    taken_names,
                )
            }
        }
    }
}

fn var_to_err_type(subs: &mut Subs, state: &mut ErrorTypeState, var: Variable) -> ErrorType {
    let desc = subs.get(var);

    if desc.mark == Mark::OCCURS {
        ErrorType::Infinite
    } else {
        subs.set_mark(var, Mark::OCCURS);

        let err_type = content_to_err_type(subs, state, var, desc.content);

        subs.set_mark(var, desc.mark);

        err_type
    }
}

fn content_to_err_type(
    subs: &mut Subs,
    state: &mut ErrorTypeState,
    var: Variable,
    content: Content,
) -> ErrorType {
    use self::Content::*;

    match content {
        Structure(flat_type) => flat_type_to_err_type(subs, state, flat_type),

        FlexVar(opt_name) => {
            let name = match opt_name {
                Some(name_index) => subs.field_names[name_index.index as usize].clone(),
                None => {
                    // set the name so when this variable occurs elsewhere in the type it gets the same name
                    let name = get_fresh_var_name(state);
                    let name_index = SubsIndex::push_new(&mut subs.field_names, name.clone());

                    subs.set_content(var, FlexVar(Some(name_index)));

                    name
                }
            };

            ErrorType::FlexVar(name)
        }

        RigidVar(name_index) => {
            let name = subs.field_names[name_index.index as usize].clone();
            ErrorType::RigidVar(name)
        }

        FlexAbleVar(opt_name, ability) => {
            let name = match opt_name {
                Some(name_index) => subs.field_names[name_index.index as usize].clone(),
                None => {
                    // set the name so when this variable occurs elsewhere in the type it gets the same name
                    let name = get_fresh_var_name(state);
                    let name_index = SubsIndex::push_new(&mut subs.field_names, name.clone());

                    subs.set_content(var, FlexVar(Some(name_index)));

                    name
                }
            };

            ErrorType::FlexAbleVar(name, ability)
        }

        RigidAbleVar(name_index, ability) => {
            let name = subs.field_names[name_index.index as usize].clone();
            ErrorType::RigidAbleVar(name, ability)
        }

        RecursionVar {
            opt_name,
            structure,
        } => {
            let name = match opt_name {
                Some(name_index) => subs.field_names[name_index.index as usize].clone(),
                None => {
                    let name = get_fresh_var_name(state);
                    let name_index = SubsIndex::push_new(&mut subs.field_names, name.clone());

                    subs.set_content(var, FlexVar(Some(name_index)));

                    name
                }
            };

            if state.recursive_tag_unions_seen.contains(&var) {
                ErrorType::FlexVar(name)
            } else {
                var_to_err_type(subs, state, structure)
            }
        }

        Alias(symbol, args, aliased_to, kind) => {
            let err_type = var_to_err_type(subs, state, aliased_to);

            let mut err_args = Vec::with_capacity(args.len());

            for var_index in args.into_iter() {
                let var = subs[var_index];

                let arg = var_to_err_type(subs, state, var);

                err_args.push(arg);
            }

            ErrorType::Alias(symbol, err_args, Box::new(err_type), kind)
        }

        RangedNumber(typ, range) => {
            let err_type = var_to_err_type(subs, state, typ);

            if state.context == ErrorTypeContext::ExpandRanges {
                let mut types = Vec::with_capacity(range.len());
                for var_index in range {
                    let var = subs[var_index];

                    types.push(var_to_err_type(subs, state, var));
                }
                ErrorType::Range(Box::new(err_type), types)
            } else {
                err_type
            }
        }

        Error => ErrorType::Error,
    }
}

fn flat_type_to_err_type(
    subs: &mut Subs,
    state: &mut ErrorTypeState,
    flat_type: FlatType,
) -> ErrorType {
    use self::FlatType::*;

    match flat_type {
        Apply(symbol, args) => {
            let arg_types = args
                .into_iter()
                .map(|index| {
                    let arg_var = subs[index];
                    var_to_err_type(subs, state, arg_var)
                })
                .collect();

            ErrorType::Type(symbol, arg_types)
        }

        Func(arg_vars, closure_var, ret_var) => {
            let args = arg_vars
                .into_iter()
                .map(|index| {
                    let arg_var = subs[index];
                    var_to_err_type(subs, state, arg_var)
                })
                .collect();

            let ret = var_to_err_type(subs, state, ret_var);
            let closure = var_to_err_type(subs, state, closure_var);

            ErrorType::Function(args, Box::new(closure), Box::new(ret))
        }

        EmptyRecord => ErrorType::Record(SendMap::default(), TypeExt::Closed),
        EmptyTagUnion => ErrorType::TagUnion(SendMap::default(), TypeExt::Closed),

        Record(vars_by_field, ext_var) => {
            let mut err_fields = SendMap::default();

            for (i1, i2, i3) in vars_by_field.iter_all() {
                let label = subs[i1].clone();
                let var = subs[i2];
                let record_field = subs[i3];

                let error_type = var_to_err_type(subs, state, var);

                use RecordField::*;
                let err_record_field = match record_field {
                    Optional(_) => Optional(error_type),
                    Required(_) => Required(error_type),
                    Demanded(_) => Demanded(error_type),
                };

                err_fields.insert(label, err_record_field);
            }

            match var_to_err_type(subs, state, ext_var).unwrap_structural_alias() {
                ErrorType::Record(sub_fields, sub_ext) => {
                    ErrorType::Record(sub_fields.union(err_fields), sub_ext)
                }

                ErrorType::FlexVar(var) => {
                    ErrorType::Record(err_fields, TypeExt::FlexOpen(var))
                }

                ErrorType::RigidVar(var) => {
                    ErrorType::Record(err_fields, TypeExt::RigidOpen(var))
                }

                other =>
                    panic!("Tried to convert a record extension to an error, but the record extension had the ErrorType of {:?}", other)
            }
        }

        TagUnion(tags, ext_var) => {
            let mut err_tags = SendMap::default();

            for (name_index, slice_index) in tags.iter_all() {
                let mut err_vars = Vec::with_capacity(tags.len());

                let slice = subs[slice_index];
                for var_index in slice {
                    let var = subs[var_index];
                    err_vars.push(var_to_err_type(subs, state, var));
                }

                let tag = subs[name_index].clone();
                err_tags.insert(tag, err_vars);
            }

            match var_to_err_type(subs, state, ext_var).unwrap_structural_alias() {
                ErrorType::TagUnion(sub_tags, sub_ext) => {
                    ErrorType::TagUnion(sub_tags.union(err_tags), sub_ext)
                }
                ErrorType::RecursiveTagUnion(_, sub_tags, sub_ext) => {
                    ErrorType::TagUnion(sub_tags.union(err_tags), sub_ext)
                }

                ErrorType::FlexVar(var) => {
                    ErrorType::TagUnion(err_tags, TypeExt::FlexOpen(var))
                }

                ErrorType::RigidVar(var) => {
                    ErrorType::TagUnion(err_tags, TypeExt::RigidOpen(var))
                }

                other =>
                    panic!("Tried to convert a tag union extension to an error, but the tag union extension had the ErrorType of {:?}", other)
            }
        }

        FunctionOrTagUnion(tag_name, _, ext_var) => {
            let tag_name = subs[tag_name].clone();

            let mut err_tags = SendMap::default();

            err_tags.insert(tag_name, vec![]);

            match var_to_err_type(subs, state, ext_var).unwrap_structural_alias() {
                ErrorType::TagUnion(sub_tags, sub_ext) => {
                    ErrorType::TagUnion(sub_tags.union(err_tags), sub_ext)
                }
                ErrorType::RecursiveTagUnion(_, sub_tags, sub_ext) => {
                    ErrorType::TagUnion(sub_tags.union(err_tags), sub_ext)
                }

                ErrorType::FlexVar(var) => {
                    ErrorType::TagUnion(err_tags, TypeExt::FlexOpen(var))
                }

                ErrorType::RigidVar(var) => {
                    ErrorType::TagUnion(err_tags, TypeExt::RigidOpen(var))
                }

                other =>
                    panic!("Tried to convert a tag union extension to an error, but the tag union extension had the ErrorType of {:?}", other)
            }
        }

        RecursiveTagUnion(rec_var, tags, ext_var) => {
            state.recursive_tag_unions_seen.push(rec_var);

            let mut err_tags = SendMap::default();

            for (name_index, slice_index) in tags.iter_all() {
                let mut err_vars = Vec::with_capacity(tags.len());

                let slice = subs[slice_index];
                for var_index in slice {
                    let var = subs[var_index];
                    err_vars.push(var_to_err_type(subs, state, var));
                }

                let tag = subs[name_index].clone();
                err_tags.insert(tag, err_vars);
            }

            let rec_error_type = Box::new(var_to_err_type(subs, state, rec_var));

            match var_to_err_type(subs, state, ext_var).unwrap_structural_alias() {
                ErrorType::RecursiveTagUnion(rec_var, sub_tags, sub_ext) => {
                    debug_assert!(rec_var == rec_error_type);
                    ErrorType::RecursiveTagUnion(rec_error_type, sub_tags.union(err_tags), sub_ext)
                }

                ErrorType::TagUnion(sub_tags, sub_ext) => {
                    ErrorType::RecursiveTagUnion(rec_error_type, sub_tags.union(err_tags), sub_ext)
                }

                ErrorType::FlexVar(var) => {
                    ErrorType::RecursiveTagUnion(rec_error_type, err_tags, TypeExt::FlexOpen(var))
                }

                ErrorType::RigidVar(var) => {
                    ErrorType::RecursiveTagUnion(rec_error_type, err_tags, TypeExt::RigidOpen(var))
                }

                other =>
                    panic!("Tried to convert a recursive tag union extension to an error, but the tag union extension had the ErrorType of {:?}", other)
            }
        }

        Erroneous(problem_index) => {
            let problem = subs.problems[problem_index.index as usize].clone();
            state.problems.push(problem);

            ErrorType::Error
        }
    }
}

fn get_fresh_var_name(state: &mut ErrorTypeState) -> Lowercase {
    let (name, new_index) =
        name_type_var(state.letters_used, &mut state.taken.iter(), |var, str| {
            var.as_str() == str
        });

    state.letters_used = new_index;

    state.taken.insert(name.clone());

    name
}

fn restore_help(subs: &mut Subs, initial: Variable) {
    let mut stack = vec![initial];

    let variable_slices = &subs.variable_slices;

    let variables = &subs.variables;
    let var_slice =
        |variable_subs_slice: VariableSubsSlice| &variables[variable_subs_slice.indices()];

    while let Some(var) = stack.pop() {
        let desc = &mut subs.utable.probe_value_ref_mut(var).value;

        if desc.copy.is_some() {
            desc.rank = Rank::NONE;
            desc.mark = Mark::NONE;
            desc.copy = OptVariable::NONE;

            use Content::*;
            use FlatType::*;

            match &desc.content {
                FlexVar(_) | RigidVar(_) | FlexAbleVar(_, _) | RigidAbleVar(_, _) | Error => (),

                RecursionVar { structure, .. } => {
                    stack.push(*structure);
                }

                Structure(flat_type) => match flat_type {
                    Apply(_, args) => {
                        stack.extend(var_slice(*args));
                    }

                    Func(arg_vars, closure_var, ret_var) => {
                        stack.extend(var_slice(*arg_vars));

                        stack.push(*ret_var);
                        stack.push(*closure_var);
                    }

                    EmptyRecord => (),
                    EmptyTagUnion => (),

                    Record(fields, ext_var) => {
                        stack.extend(var_slice(fields.variables()));

                        stack.push(*ext_var);
                    }
                    TagUnion(tags, ext_var) => {
                        for slice_index in tags.variables() {
                            let slice = variable_slices[slice_index.index as usize];
                            stack.extend(var_slice(slice));
                        }

                        stack.push(*ext_var);
                    }
                    FunctionOrTagUnion(_, _, ext_var) => {
                        stack.push(*ext_var);
                    }

                    RecursiveTagUnion(rec_var, tags, ext_var) => {
                        for slice_index in tags.variables() {
                            let slice = variable_slices[slice_index.index as usize];
                            stack.extend(var_slice(slice));
                        }

                        stack.push(*ext_var);
                        stack.push(*rec_var);
                    }

                    Erroneous(_) => (),
                },
                Alias(_, args, var, _) => {
                    stack.extend(var_slice(args.all_variables()));

                    stack.push(*var);
                }

                RangedNumber(typ, vars) => {
                    stack.push(*typ);
                    stack.extend(var_slice(*vars));
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct StorageSubs {
    subs: Subs,
}

#[derive(Copy, Clone, Debug)]
struct StorageSubsOffsets {
    utable: u32,
    variables: u32,
    tag_names: u32,
    field_names: u32,
    record_fields: u32,
    variable_slices: u32,
    problems: u32,
}

impl StorageSubs {
    pub fn new(subs: Subs) -> Self {
        Self { subs }
    }

    pub fn fresh_unnamed_flex_var(&mut self) -> Variable {
        self.subs.fresh_unnamed_flex_var()
    }

    pub fn as_inner_mut(&mut self) -> &mut Subs {
        &mut self.subs
    }

    pub fn extend_with_variable(&mut self, source: &mut Subs, variable: Variable) -> Variable {
        deep_copy_var_to(source, &mut self.subs, variable)
    }

    pub fn import_variable_from(&mut self, source: &mut Subs, variable: Variable) -> CopiedImport {
        copy_import_to(source, &mut self.subs, variable, Rank::import())
    }

    pub fn export_variable_to(&mut self, target: &mut Subs, variable: Variable) -> CopiedImport {
        copy_import_to(&mut self.subs, target, variable, Rank::import())
    }

    pub fn merge_into(self, target: &mut Subs) -> impl Fn(Variable) -> Variable {
        let self_offsets = StorageSubsOffsets {
            utable: self.subs.utable.len() as u32,
            variables: self.subs.variables.len() as u32,
            tag_names: self.subs.tag_names.len() as u32,
            field_names: self.subs.field_names.len() as u32,
            record_fields: self.subs.record_fields.len() as u32,
            variable_slices: self.subs.variable_slices.len() as u32,
            problems: self.subs.problems.len() as u32,
        };

        let offsets = StorageSubsOffsets {
            utable: (target.utable.len() - Variable::NUM_RESERVED_VARS) as u32,
            variables: target.variables.len() as u32,
            tag_names: target.tag_names.len() as u32,
            field_names: target.field_names.len() as u32,
            record_fields: target.record_fields.len() as u32,
            variable_slices: target.variable_slices.len() as u32,
            problems: target.problems.len() as u32,
        };

        // The first Variable::NUM_RESERVED_VARS are the same in every subs,
        // so we can skip copying them!
        let range = Variable::NUM_RESERVED_VARS..self.subs.utable.len();

        // fill new slots with empty values
        target.extend_by(range.len());

        for i in range {
            let variable = Variable(i as u32);
            let descriptor = self.subs.get_ref(variable);
            debug_assert!(descriptor.copy.is_none());

            let new_content = Self::offset_content(&offsets, &descriptor.content);

            let new_descriptor = Descriptor {
                rank: descriptor.rank,
                mark: descriptor.mark,
                copy: OptVariable::NONE,
                content: new_content,
            };

            let new_variable = Self::offset_variable(&offsets, variable);
            target.set(new_variable, new_descriptor);
        }

        target.variables.extend(
            self.subs
                .variables
                .iter()
                .map(|v| Self::offset_variable(&offsets, *v)),
        );

        target.variable_slices.extend(
            self.subs
                .variable_slices
                .into_iter()
                .map(|v| Self::offset_variable_slice(&offsets, v)),
        );

        target.tag_names.extend(self.subs.tag_names);
        target.field_names.extend(self.subs.field_names);
        target.record_fields.extend(self.subs.record_fields);
        target.problems.extend(self.subs.problems);

        debug_assert_eq!(
            target.utable.len(),
            (self_offsets.utable + offsets.utable) as usize
        );

        debug_assert_eq!(
            target.tag_names.len(),
            (self_offsets.tag_names + offsets.tag_names) as usize
        );

        move |v| {
            let offsets = offsets;
            Self::offset_variable(&offsets, v)
        }
    }

    fn offset_flat_type(offsets: &StorageSubsOffsets, flat_type: &FlatType) -> FlatType {
        match flat_type {
            FlatType::Apply(symbol, arguments) => {
                FlatType::Apply(*symbol, Self::offset_variable_slice(offsets, *arguments))
            }
            FlatType::Func(arguments, lambda_set, result) => FlatType::Func(
                Self::offset_variable_slice(offsets, *arguments),
                Self::offset_variable(offsets, *lambda_set),
                Self::offset_variable(offsets, *result),
            ),
            FlatType::Record(record_fields, ext) => FlatType::Record(
                Self::offset_record_fields(offsets, *record_fields),
                Self::offset_variable(offsets, *ext),
            ),
            FlatType::TagUnion(union_tags, ext) => FlatType::TagUnion(
                Self::offset_union_tags(offsets, *union_tags),
                Self::offset_variable(offsets, *ext),
            ),
            FlatType::FunctionOrTagUnion(tag_name, symbol, ext) => FlatType::FunctionOrTagUnion(
                Self::offset_tag_name_index(offsets, *tag_name),
                *symbol,
                Self::offset_variable(offsets, *ext),
            ),
            FlatType::RecursiveTagUnion(rec, union_tags, ext) => FlatType::RecursiveTagUnion(
                Self::offset_variable(offsets, *rec),
                Self::offset_union_tags(offsets, *union_tags),
                Self::offset_variable(offsets, *ext),
            ),
            FlatType::Erroneous(problem) => {
                FlatType::Erroneous(Self::offset_problem(offsets, *problem))
            }
            FlatType::EmptyRecord => FlatType::EmptyRecord,
            FlatType::EmptyTagUnion => FlatType::EmptyTagUnion,
        }
    }

    fn offset_content(offsets: &StorageSubsOffsets, content: &Content) -> Content {
        use Content::*;

        match content {
            FlexVar(opt_name) => FlexVar(*opt_name),
            RigidVar(name) => RigidVar(*name),
            FlexAbleVar(opt_name, ability) => FlexAbleVar(*opt_name, *ability),
            RigidAbleVar(name, ability) => RigidAbleVar(*name, *ability),
            RecursionVar {
                structure,
                opt_name,
            } => RecursionVar {
                structure: Self::offset_variable(offsets, *structure),
                opt_name: *opt_name,
            },
            Structure(flat_type) => Structure(Self::offset_flat_type(offsets, flat_type)),
            Alias(symbol, alias_variables, actual, kind) => Alias(
                *symbol,
                Self::offset_alias_variables(offsets, *alias_variables),
                Self::offset_variable(offsets, *actual),
                *kind,
            ),
            RangedNumber(typ, vars) => RangedNumber(
                Self::offset_variable(offsets, *typ),
                Self::offset_variable_slice(offsets, *vars),
            ),
            Error => Content::Error,
        }
    }

    fn offset_alias_variables(
        offsets: &StorageSubsOffsets,
        mut alias_variables: AliasVariables,
    ) -> AliasVariables {
        alias_variables.variables_start += offsets.variables;

        alias_variables
    }

    fn offset_union_tags(offsets: &StorageSubsOffsets, mut union_tags: UnionTags) -> UnionTags {
        union_tags.tag_names_start += offsets.tag_names;
        union_tags.variables_start += offsets.variable_slices;

        union_tags
    }

    fn offset_record_fields(
        offsets: &StorageSubsOffsets,
        mut record_fields: RecordFields,
    ) -> RecordFields {
        record_fields.field_names_start += offsets.field_names;
        record_fields.variables_start += offsets.variables;
        record_fields.field_types_start += offsets.record_fields;

        record_fields
    }

    fn offset_tag_name_index(
        offsets: &StorageSubsOffsets,
        mut tag_name: SubsIndex<TagName>,
    ) -> SubsIndex<TagName> {
        tag_name.index += offsets.tag_names;

        tag_name
    }

    fn offset_variable(offsets: &StorageSubsOffsets, variable: Variable) -> Variable {
        if variable.index() < Variable::FIRST_USER_SPACE_VAR.index() {
            variable
        } else {
            let new_index = variable.0 + offsets.utable;
            Variable(new_index)
        }
    }

    fn offset_variable_slice(
        offsets: &StorageSubsOffsets,
        mut slice: VariableSubsSlice,
    ) -> VariableSubsSlice {
        slice.start += offsets.variables;

        slice
    }

    fn offset_problem(
        offsets: &StorageSubsOffsets,
        mut problem_index: SubsIndex<Problem>,
    ) -> SubsIndex<Problem> {
        problem_index.index += offsets.problems;

        problem_index
    }
}

use std::cell::RefCell;
std::thread_local! {
    /// Scratchpad arena so we don't need to allocate a new one all the time
    static SCRATCHPAD: RefCell<Option<bumpalo::Bump>> = RefCell::new(Some(bumpalo::Bump::with_capacity(4 * 1024)));
}

fn take_scratchpad() -> bumpalo::Bump {
    SCRATCHPAD.with(|f| f.take().unwrap())
}

fn put_scratchpad(scratchpad: bumpalo::Bump) {
    SCRATCHPAD.with(|f| {
        f.replace(Some(scratchpad));
    });
}

pub fn deep_copy_var_to(
    source: &mut Subs, // mut to set the copy
    target: &mut Subs,
    var: Variable,
) -> Variable {
    let rank = Rank::toplevel();

    let mut arena = take_scratchpad();

    let copy = {
        let visited = bumpalo::collections::Vec::with_capacity_in(256, &arena);

        let mut env = DeepCopyVarToEnv {
            visited,
            source,
            target,
            max_rank: rank,
        };

        let copy = deep_copy_var_to_help(&mut env, var);

        // we have tracked all visited variables, and can now traverse them
        // in one go (without looking at the UnificationTable) and clear the copy field
        for var in env.visited {
            let descriptor = env.source.get_ref_mut(var);

            if descriptor.copy.is_some() {
                descriptor.rank = Rank::NONE;
                descriptor.mark = Mark::NONE;
                descriptor.copy = OptVariable::NONE;
            }
        }

        copy
    };

    arena.reset();
    put_scratchpad(arena);

    copy
}

struct DeepCopyVarToEnv<'a> {
    visited: bumpalo::collections::Vec<'a, Variable>,
    source: &'a mut Subs,
    target: &'a mut Subs,
    max_rank: Rank,
}

fn deep_copy_var_to_help(env: &mut DeepCopyVarToEnv<'_>, var: Variable) -> Variable {
    use Content::*;
    use FlatType::*;

    let desc = env.source.get_without_compacting(var);

    if let Some(copy) = desc.copy.into_variable() {
        debug_assert!(env.target.contains(copy));
        return copy;
    } else if desc.rank != Rank::NONE {
        // DO NOTHING, Fall through
        //
        // The original deep_copy_var can do
        // return var;
        //
        // but we cannot, because this `var` is in the source, not the target, and we
        // should only return variables in the target. so, we have to create a new
        // variable in the target.
    }

    env.visited.push(var);

    let max_rank = env.max_rank;

    let make_descriptor = |content| Descriptor {
        content,
        rank: max_rank,
        mark: Mark::NONE,
        copy: OptVariable::NONE,
    };

    let copy = env.target.fresh_unnamed_flex_var();

    // Link the original variable to the new variable. This lets us
    // avoid making multiple copies of the variable we are instantiating.
    //
    // Need to do this before recursively copying to avoid looping.
    env.source.modify(var, |descriptor| {
        descriptor.mark = Mark::NONE;
        descriptor.copy = copy.into();
    });

    // Now we recursively copy the content of the variable.
    // We have already marked the variable as copied, so we
    // will not repeat this work or crawl this variable again.
    match desc.content {
        Structure(flat_type) => {
            let new_flat_type = match flat_type {
                Apply(symbol, arguments) => {
                    let new_arguments = SubsSlice::reserve_into_subs(env.target, arguments.len());

                    for (target_index, var_index) in (new_arguments.indices()).zip(arguments) {
                        let var = env.source[var_index];
                        let copy_var = deep_copy_var_to_help(env, var);
                        env.target.variables[target_index] = copy_var;
                    }

                    Apply(symbol, new_arguments)
                }

                Func(arguments, closure_var, ret_var) => {
                    let new_ret_var = deep_copy_var_to_help(env, ret_var);

                    let new_closure_var = deep_copy_var_to_help(env, closure_var);

                    let new_arguments = SubsSlice::reserve_into_subs(env.target, arguments.len());

                    for (target_index, var_index) in (new_arguments.indices()).zip(arguments) {
                        let var = env.source[var_index];
                        let copy_var = deep_copy_var_to_help(env, var);
                        env.target.variables[target_index] = copy_var;
                    }

                    Func(new_arguments, new_closure_var, new_ret_var)
                }

                same @ EmptyRecord | same @ EmptyTagUnion | same @ Erroneous(_) => same,

                Record(fields, ext_var) => {
                    let record_fields = {
                        let new_variables =
                            VariableSubsSlice::reserve_into_subs(env.target, fields.len());

                        let it = (new_variables.indices()).zip(fields.iter_variables());
                        for (target_index, var_index) in it {
                            let var = env.source[var_index];
                            let copy_var = deep_copy_var_to_help(env, var);
                            env.target.variables[target_index] = copy_var;
                        }

                        let field_names_start = env.target.field_names.len() as u32;
                        let field_types_start = env.target.record_fields.len() as u32;

                        let field_names = &env.source.field_names[fields.field_names().indices()];
                        env.target.field_names.extend(field_names.iter().cloned());

                        let record_fields =
                            &env.source.record_fields[fields.record_fields().indices()];
                        env.target
                            .record_fields
                            .extend(record_fields.iter().copied());

                        RecordFields {
                            length: fields.len() as _,
                            field_names_start,
                            variables_start: new_variables.start,
                            field_types_start,
                        }
                    };

                    Record(record_fields, deep_copy_var_to_help(env, ext_var))
                }

                TagUnion(tags, ext_var) => {
                    let new_ext = deep_copy_var_to_help(env, ext_var);

                    let new_variable_slices =
                        SubsSlice::reserve_variable_slices(env.target, tags.len());

                    let it = (new_variable_slices.indices()).zip(tags.variables());
                    for (target_index, index) in it {
                        let slice = env.source[index];

                        let new_variables = SubsSlice::reserve_into_subs(env.target, slice.len());
                        let it = (new_variables.indices()).zip(slice);
                        for (target_index, var_index) in it {
                            let var = env.source[var_index];
                            let copy_var = deep_copy_var_to_help(env, var);
                            env.target.variables[target_index] = copy_var;
                        }

                        env.target.variable_slices[target_index] = new_variables;
                    }

                    let new_tag_names = {
                        let tag_names = tags.tag_names();
                        let slice = &env.source.tag_names[tag_names.indices()];

                        let start = env.target.tag_names.len() as u32;
                        let length = tag_names.len() as u16;

                        env.target.tag_names.extend(slice.iter().cloned());

                        SubsSlice::new(start, length)
                    };

                    let union_tags = UnionTags::from_slices(new_tag_names, new_variable_slices);

                    TagUnion(union_tags, new_ext)
                }

                FunctionOrTagUnion(tag_name, symbol, ext_var) => {
                    let new_tag_name = SubsIndex::new(env.target.tag_names.len() as u32);

                    env.target.tag_names.push(env.source[tag_name].clone());

                    FunctionOrTagUnion(new_tag_name, symbol, deep_copy_var_to_help(env, ext_var))
                }

                RecursiveTagUnion(rec_var, tags, ext_var) => {
                    let new_variable_slices =
                        SubsSlice::reserve_variable_slices(env.target, tags.len());

                    let it = (new_variable_slices.indices()).zip(tags.variables());
                    for (target_index, index) in it {
                        let slice = env.source[index];

                        let new_variables = SubsSlice::reserve_into_subs(env.target, slice.len());
                        let it = (new_variables.indices()).zip(slice);
                        for (target_index, var_index) in it {
                            let var = env.source[var_index];
                            let copy_var = deep_copy_var_to_help(env, var);
                            env.target.variables[target_index] = copy_var;
                        }

                        env.target.variable_slices[target_index] = new_variables;
                    }

                    let new_tag_names = {
                        let tag_names = tags.tag_names();
                        let slice = &env.source.tag_names[tag_names.indices()];

                        let start = env.target.tag_names.len() as u32;
                        let length = tag_names.len() as u16;

                        env.target.tag_names.extend(slice.iter().cloned());

                        SubsSlice::new(start, length)
                    };

                    let union_tags = UnionTags::from_slices(new_tag_names, new_variable_slices);

                    let new_ext = deep_copy_var_to_help(env, ext_var);
                    let new_rec_var = deep_copy_var_to_help(env, rec_var);

                    RecursiveTagUnion(new_rec_var, union_tags, new_ext)
                }
            };

            env.target
                .set(copy, make_descriptor(Structure(new_flat_type)));

            copy
        }

        FlexVar(Some(name_index)) => {
            let name = env.source.field_names[name_index.index as usize].clone();
            let new_name_index = SubsIndex::push_new(&mut env.target.field_names, name);

            let content = FlexVar(Some(new_name_index));
            env.target.set_content(copy, content);

            copy
        }

        FlexVar(None) | Error => copy,

        RecursionVar {
            opt_name,
            structure,
        } => {
            let new_structure = deep_copy_var_to_help(env, structure);

            debug_assert!((new_structure.index() as usize) < env.target.len());

            env.target.set(
                copy,
                make_descriptor(RecursionVar {
                    opt_name,
                    structure: new_structure,
                }),
            );

            copy
        }

        RigidVar(name_index) => {
            let name = env.source.field_names[name_index.index as usize].clone();
            let new_name_index = SubsIndex::push_new(&mut env.target.field_names, name);
            env.target
                .set(copy, make_descriptor(FlexVar(Some(new_name_index))));

            copy
        }

        FlexAbleVar(opt_name_index, ability) => {
            let new_name_index = opt_name_index.map(|name_index| {
                let name = env.source.field_names[name_index.index as usize].clone();
                SubsIndex::push_new(&mut env.target.field_names, name)
            });

            let content = FlexAbleVar(new_name_index, ability);
            env.target.set_content(copy, content);

            copy
        }

        RigidAbleVar(name_index, ability) => {
            let name = env.source.field_names[name_index.index as usize].clone();
            let new_name_index = SubsIndex::push_new(&mut env.target.field_names, name);
            env.target.set(
                copy,
                make_descriptor(FlexAbleVar(Some(new_name_index), ability)),
            );

            copy
        }

        Alias(symbol, arguments, real_type_var, kind) => {
            let new_variables =
                SubsSlice::reserve_into_subs(env.target, arguments.all_variables_len as _);
            for (target_index, var_index) in
                (new_variables.indices()).zip(arguments.all_variables())
            {
                let var = env.source[var_index];
                let copy_var = deep_copy_var_to_help(env, var);
                env.target.variables[target_index] = copy_var;
            }

            let new_arguments = AliasVariables {
                variables_start: new_variables.start,
                ..arguments
            };

            let new_real_type_var = deep_copy_var_to_help(env, real_type_var);
            let new_content = Alias(symbol, new_arguments, new_real_type_var, kind);

            env.target.set(copy, make_descriptor(new_content));

            copy
        }

        RangedNumber(typ, vars) => {
            let new_typ = deep_copy_var_to_help(env, typ);

            let new_vars = SubsSlice::reserve_into_subs(env.target, vars.len());

            for (target_index, var_index) in (new_vars.indices()).zip(vars) {
                let var = env.source[var_index];
                let copy_var = deep_copy_var_to_help(env, var);
                env.target.variables[target_index] = copy_var;
            }

            let new_content = RangedNumber(new_typ, new_vars);

            env.target.set(copy, make_descriptor(new_content));
            copy
        }
    }
}

/// Bookkeeping to correctly move these types into the target subs
///
/// We track the rigid/flex variables because they need to be part of a `Let`
/// constraint, introducing these variables at the right rank
///
/// We also track `registered` variables. An import should be equivalent to
/// a call to `type_to_var` (solve.rs). The `copy_import_to` function puts
/// the right `Contents` into the target `Subs` at the right locations,
/// but `type_to_var` furthermore adds the variables used to store those `Content`s
/// to `Pools` at the right rank. Here we remember the variables used to store `Content`s
/// so that we can later add them to `Pools`
#[derive(Debug)]
pub struct CopiedImport {
    pub variable: Variable,
    pub flex: Vec<Variable>,
    pub rigid: Vec<Variable>,
    pub flex_able: Vec<Variable>,
    pub rigid_able: Vec<Variable>,
    pub translations: Vec<(Variable, Variable)>,
    pub registered: Vec<Variable>,
}

struct CopyImportEnv<'a> {
    visited: bumpalo::collections::Vec<'a, Variable>,
    source: &'a mut Subs,
    target: &'a mut Subs,
    flex: Vec<Variable>,
    rigid: Vec<Variable>,
    flex_able: Vec<Variable>,
    rigid_able: Vec<Variable>,
    translations: Vec<(Variable, Variable)>,
    registered: Vec<Variable>,
}

pub fn copy_import_to(
    source: &mut Subs, // mut to set the copy
    target: &mut Subs,
    var: Variable,
    rank: Rank,
) -> CopiedImport {
    let mut arena = take_scratchpad();

    let copied_import = {
        let visited = bumpalo::collections::Vec::with_capacity_in(256, &arena);

        let mut env = CopyImportEnv {
            visited,
            source,
            target,
            flex: Vec::new(),
            rigid: Vec::new(),
            flex_able: Vec::new(),
            rigid_able: Vec::new(),
            translations: Vec::new(),
            registered: Vec::new(),
        };

        let copy = copy_import_to_help(&mut env, rank, var);

        let CopyImportEnv {
            visited,
            source,
            flex,
            rigid,
            flex_able,
            rigid_able,
            translations,
            registered,
            target: _,
        } = env;

        // we have tracked all visited variables, and can now traverse them
        // in one go (without looking at the UnificationTable) and clear the copy field

        for var in visited {
            let descriptor = source.get_ref_mut(var);

            if descriptor.copy.is_some() {
                descriptor.rank = Rank::NONE;
                descriptor.mark = Mark::NONE;
                descriptor.copy = OptVariable::NONE;
            }
        }

        CopiedImport {
            variable: copy,
            flex,
            rigid,
            flex_able,
            rigid_able,
            translations,
            registered,
        }
    };

    arena.reset();
    put_scratchpad(arena);

    copied_import
}

/// is this content registered (in the current pool) by type_to_variable?
/// TypeToVar skips registering for flex and rigid variables, and
/// also for the empty records and tag unions (they used the Variable::EMPTY_RECORD/...)
/// standard variables
fn is_registered(content: &Content) -> bool {
    match content {
        Content::FlexVar(_)
        | Content::RigidVar(_)
        | Content::FlexAbleVar(..)
        | Content::RigidAbleVar(..) => false,
        Content::Structure(FlatType::EmptyRecord | FlatType::EmptyTagUnion) => false,

        Content::Structure(_)
        | Content::RecursionVar { .. }
        | Content::Alias(_, _, _, _)
        | Content::RangedNumber(_, _)
        | Content::Error => true,
    }
}

fn copy_import_to_help(env: &mut CopyImportEnv<'_>, max_rank: Rank, var: Variable) -> Variable {
    use Content::*;
    use FlatType::*;

    let desc = env.source.get_without_compacting(var);

    if let Some(copy) = desc.copy.into_variable() {
        debug_assert!(env.target.contains(copy));
        return copy;
    } else if desc.rank != Rank::NONE {
        // DO NOTHING, Fall through
        //
        // The original copy_import can do
        // return var;
        //
        // but we cannot, because this `var` is in the source, not the target, and we
        // should only return variables in the target. so, we have to create a new
        // variable in the target.
    }

    env.visited.push(var);

    let make_descriptor = |content| Descriptor {
        content,
        rank: max_rank,
        mark: Mark::NONE,
        copy: OptVariable::NONE,
    };

    // let copy = env.target.fresh_unnamed_flex_var();
    let copy = env.target.fresh(make_descriptor(unnamed_flex_var()));

    // is this content registered (in the current pool) by type_to_variable?
    if is_registered(&desc.content) {
        env.registered.push(copy);
    }

    // Link the original variable to the new variable. This lets us
    // avoid making multiple copies of the variable we are instantiating.
    //
    // Need to do this before recursively copying to avoid looping.
    env.source.modify(var, |descriptor| {
        descriptor.mark = Mark::NONE;
        descriptor.copy = copy.into();
    });

    // Now we recursively copy the content of the variable.
    // We have already marked the variable as copied, so we
    // will not repeat this work or crawl this variable again.
    match desc.content {
        Structure(Erroneous(_)) => {
            // Make this into a flex var so that we don't have to copy problems across module
            // boundaries - the error will be reported locally.
            env.target.set(copy, make_descriptor(FlexVar(None)));

            copy
        }
        Structure(flat_type) => {
            let new_flat_type = match flat_type {
                Apply(symbol, arguments) => {
                    let new_arguments = SubsSlice::reserve_into_subs(env.target, arguments.len());

                    for (target_index, var_index) in (new_arguments.indices()).zip(arguments) {
                        let var = env.source[var_index];
                        let copy_var = copy_import_to_help(env, max_rank, var);
                        env.target.variables[target_index] = copy_var;
                    }

                    Apply(symbol, new_arguments)
                }

                Func(arguments, closure_var, ret_var) => {
                    let new_ret_var = copy_import_to_help(env, max_rank, ret_var);

                    let new_closure_var = copy_import_to_help(env, max_rank, closure_var);

                    let new_arguments = SubsSlice::reserve_into_subs(env.target, arguments.len());

                    for (target_index, var_index) in (new_arguments.indices()).zip(arguments) {
                        let var = env.source[var_index];
                        let copy_var = copy_import_to_help(env, max_rank, var);
                        env.target.variables[target_index] = copy_var;
                    }

                    Func(new_arguments, new_closure_var, new_ret_var)
                }

                Erroneous(_) => internal_error!("I thought this was handled above"),

                same @ EmptyRecord | same @ EmptyTagUnion => same,

                Record(fields, ext_var) => {
                    let record_fields = {
                        let new_variables =
                            VariableSubsSlice::reserve_into_subs(env.target, fields.len());

                        let it = (new_variables.indices()).zip(fields.iter_variables());
                        for (target_index, var_index) in it {
                            let var = env.source[var_index];
                            let copy_var = copy_import_to_help(env, max_rank, var);
                            env.target.variables[target_index] = copy_var;
                        }

                        let field_names_start = env.target.field_names.len() as u32;
                        let field_types_start = env.target.record_fields.len() as u32;

                        let field_names = &env.source.field_names[fields.field_names().indices()];
                        env.target.field_names.extend(field_names.iter().cloned());

                        let record_fields =
                            &env.source.record_fields[fields.record_fields().indices()];
                        env.target
                            .record_fields
                            .extend(record_fields.iter().copied());

                        RecordFields {
                            length: fields.len() as _,
                            field_names_start,
                            variables_start: new_variables.start,
                            field_types_start,
                        }
                    };

                    Record(record_fields, copy_import_to_help(env, max_rank, ext_var))
                }

                TagUnion(tags, ext_var) => {
                    let new_ext = copy_import_to_help(env, max_rank, ext_var);

                    let new_variable_slices =
                        SubsSlice::reserve_variable_slices(env.target, tags.len());

                    let it = (new_variable_slices.indices()).zip(tags.variables());
                    for (target_index, index) in it {
                        let slice = env.source[index];

                        let new_variables = SubsSlice::reserve_into_subs(env.target, slice.len());
                        let it = (new_variables.indices()).zip(slice);
                        for (target_index, var_index) in it {
                            let var = env.source[var_index];
                            let copy_var = copy_import_to_help(env, max_rank, var);
                            env.target.variables[target_index] = copy_var;
                        }

                        env.target.variable_slices[target_index] = new_variables;
                    }

                    let new_tag_names = {
                        let tag_names = tags.tag_names();
                        let slice = &env.source.tag_names[tag_names.indices()];

                        let start = env.target.tag_names.len() as u32;
                        let length = tag_names.len() as u16;

                        env.target.tag_names.extend(slice.iter().cloned());

                        SubsSlice::new(start, length)
                    };

                    let union_tags = UnionTags::from_slices(new_tag_names, new_variable_slices);

                    TagUnion(union_tags, new_ext)
                }

                FunctionOrTagUnion(tag_name, symbol, ext_var) => {
                    let new_tag_name = SubsIndex::new(env.target.tag_names.len() as u32);

                    env.target.tag_names.push(env.source[tag_name].clone());

                    FunctionOrTagUnion(
                        new_tag_name,
                        symbol,
                        copy_import_to_help(env, max_rank, ext_var),
                    )
                }

                RecursiveTagUnion(rec_var, tags, ext_var) => {
                    let new_variable_slices =
                        SubsSlice::reserve_variable_slices(env.target, tags.len());

                    let it = (new_variable_slices.indices()).zip(tags.variables());
                    for (target_index, index) in it {
                        let slice = env.source[index];

                        let new_variables = SubsSlice::reserve_into_subs(env.target, slice.len());
                        let it = (new_variables.indices()).zip(slice);
                        for (target_index, var_index) in it {
                            let var = env.source[var_index];
                            let copy_var = copy_import_to_help(env, max_rank, var);
                            env.target.variables[target_index] = copy_var;
                        }

                        env.target.variable_slices[target_index] = new_variables;
                    }

                    let new_tag_names = {
                        let tag_names = tags.tag_names();
                        let slice = &env.source.tag_names[tag_names.indices()];

                        let start = env.target.tag_names.len() as u32;
                        let length = tag_names.len() as u16;

                        env.target.tag_names.extend(slice.iter().cloned());

                        SubsSlice::new(start, length)
                    };

                    let union_tags = UnionTags::from_slices(new_tag_names, new_variable_slices);

                    let new_ext = copy_import_to_help(env, max_rank, ext_var);
                    let new_rec_var = copy_import_to_help(env, max_rank, rec_var);

                    RecursiveTagUnion(new_rec_var, union_tags, new_ext)
                }
            };

            env.target
                .set(copy, make_descriptor(Structure(new_flat_type)));

            copy
        }

        FlexVar(opt_name_index) => {
            if let Some(name_index) = opt_name_index {
                let name = env.source.field_names[name_index.index as usize].clone();
                let new_name_index = SubsIndex::push_new(&mut env.target.field_names, name);

                let content = FlexVar(Some(new_name_index));
                env.target.set_content(copy, content);
            }

            env.flex.push(copy);

            copy
        }

        FlexAbleVar(opt_name_index, ability) => {
            if let Some(name_index) = opt_name_index {
                let name = env.source.field_names[name_index.index as usize].clone();
                let new_name_index = SubsIndex::push_new(&mut env.target.field_names, name);

                let content = FlexAbleVar(Some(new_name_index), ability);
                env.target.set_content(copy, content);
            }

            env.flex_able.push(copy);

            copy
        }

        Error => {
            // Open question: should this return Error, or a Flex var?

            env.target.set(copy, make_descriptor(Error));

            copy
        }

        RigidVar(name_index) => {
            let name = env.source.field_names[name_index.index as usize].clone();
            let new_name_index = SubsIndex::push_new(&mut env.target.field_names, name);

            env.target
                .set(copy, make_descriptor(RigidVar(new_name_index)));

            env.rigid.push(copy);

            env.translations.push((var, copy));

            copy
        }

        RigidAbleVar(name_index, ability) => {
            let name = env.source.field_names[name_index.index as usize].clone();
            let new_name_index = SubsIndex::push_new(&mut env.target.field_names, name);

            env.target
                .set(copy, make_descriptor(RigidAbleVar(new_name_index, ability)));

            env.rigid_able.push(copy);

            env.translations.push((var, copy));

            copy
        }

        RecursionVar {
            opt_name,
            structure,
        } => {
            let new_structure = copy_import_to_help(env, max_rank, structure);

            debug_assert!((new_structure.index() as usize) < env.target.len());

            env.target.set(
                copy,
                make_descriptor(RecursionVar {
                    opt_name,
                    structure: new_structure,
                }),
            );

            copy
        }

        Alias(symbol, arguments, real_type_var, kind) => {
            let new_variables =
                SubsSlice::reserve_into_subs(env.target, arguments.all_variables_len as _);
            for (target_index, var_index) in
                (new_variables.indices()).zip(arguments.all_variables())
            {
                let var = env.source[var_index];
                let copy_var = copy_import_to_help(env, max_rank, var);
                env.target.variables[target_index] = copy_var;
            }

            let new_arguments = AliasVariables {
                variables_start: new_variables.start,
                ..arguments
            };

            let new_real_type_var = copy_import_to_help(env, max_rank, real_type_var);
            let new_content = Alias(symbol, new_arguments, new_real_type_var, kind);

            env.target.set(copy, make_descriptor(new_content));

            copy
        }

        RangedNumber(typ, vars) => {
            let new_typ = copy_import_to_help(env, max_rank, typ);

            let new_vars = SubsSlice::reserve_into_subs(env.target, vars.len());

            for (target_index, var_index) in (new_vars.indices()).zip(vars) {
                let var = env.source[var_index];
                let copy_var = copy_import_to_help(env, max_rank, var);
                env.target.variables[target_index] = copy_var;
            }

            let new_content = RangedNumber(new_typ, new_vars);

            env.target.set(copy, make_descriptor(new_content));
            copy
        }
    }
}

fn var_contains_content_help<P>(
    subs: &Subs,
    var: Variable,
    predicate: P,
    seen_recursion_vars: &mut MutSet<Variable>,
) -> bool
where
    P: Fn(&Content) -> bool + Copy,
{
    let mut stack = vec![var];

    macro_rules! push_var_slice {
        ($slice:expr) => {
            stack.extend(subs.get_subs_slice($slice))
        };
    }

    while let Some(var) = stack.pop() {
        if seen_recursion_vars.contains(&var) {
            continue;
        }

        let content = subs.get_content_without_compacting(var);

        if predicate(content) {
            return true;
        }

        use Content::*;
        use FlatType::*;
        match content {
            FlexVar(_) | RigidVar(_) | FlexAbleVar(_, _) | RigidAbleVar(_, _) => {}
            RecursionVar {
                structure,
                opt_name: _,
            } => {
                seen_recursion_vars.insert(var);
                stack.push(*structure);
            }
            Structure(flat_type) => match flat_type {
                Apply(_, vars) => push_var_slice!(*vars),
                Func(args, clos, ret) => {
                    push_var_slice!(*args);
                    stack.push(*clos);
                    stack.push(*ret);
                }
                Record(fields, var) => {
                    push_var_slice!(fields.variables());
                    stack.push(*var);
                }
                TagUnion(tags, ext_var) => {
                    for i in tags.variables() {
                        push_var_slice!(subs[i]);
                    }
                    stack.push(*ext_var);
                }
                FunctionOrTagUnion(_, _, var) => stack.push(*var),
                RecursiveTagUnion(rec_var, tags, ext_var) => {
                    seen_recursion_vars.insert(*rec_var);
                    for i in tags.variables() {
                        push_var_slice!(subs[i]);
                    }
                    stack.push(*ext_var);
                }
                Erroneous(_) | EmptyRecord | EmptyTagUnion => {}
            },
            Alias(_, arguments, real_type_var, _) => {
                push_var_slice!(arguments.all_variables());
                stack.push(*real_type_var);
            }
            RangedNumber(typ, vars) => {
                stack.push(*typ);
                push_var_slice!(*vars);
            }
            Error => {}
        }
    }
    false
}
