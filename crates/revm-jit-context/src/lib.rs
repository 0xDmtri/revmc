#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_extern_crates))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

use core::{any::Any, fmt, mem::MaybeUninit, ptr};
use revm_interpreter::{
    Contract, Gas, Host, InstructionResult, Interpreter, InterpreterAction, InterpreterResult,
    SharedMemory,
};
use revm_primitives::{Address, Bytes, Env, U256};

#[cfg(feature = "alloc")]
use alloc::vec::Vec;

/// The JIT EVM context.
///
/// Currently contains and handler memory and the host.
pub struct EvmContext<'a> {
    /// The memory.
    pub memory: &'a mut SharedMemory,
    /// Contract information and call data.
    pub contract: &'a mut Contract,
    /// The gas.
    pub gas: &'a mut Gas,
    /// The host.
    pub host: &'a mut dyn HostExt,
    /// The return action.
    pub next_action: &'a mut InterpreterAction,
    /// The return data.
    pub return_data: &'a [u8],
    /// Whether the context is static.
    pub is_static: bool,
    /// An index that is used internally to keep track of where execution should resume.
    /// `0` is the initial state.
    #[doc(hidden)]
    pub resume_at: u32,
}

impl fmt::Debug for EvmContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvmContext").field("memory", &self.memory).finish_non_exhaustive()
    }
}

impl<'a> EvmContext<'a> {
    /// Creates a new JIT EVM context from an interpreter.
    #[inline]
    pub fn from_interpreter(interpreter: &'a mut Interpreter, host: &'a mut dyn HostExt) -> Self {
        Self::from_interpreter_with_stack(interpreter, host).0
    }

    /// Creates a new JIT EVM context from an interpreter.
    #[inline]
    pub fn from_interpreter_with_stack<'b: 'a>(
        interpreter: &'a mut Interpreter,
        host: &'b mut dyn HostExt,
    ) -> (Self, &'a mut EvmStack, &'a mut usize) {
        let (stack, stack_len) = EvmStack::from_interpreter_stack(&mut interpreter.stack);
        let this = Self {
            memory: &mut interpreter.shared_memory,
            contract: &mut interpreter.contract,
            gas: &mut interpreter.gas,
            host,
            next_action: &mut interpreter.next_action,
            return_data: &interpreter.return_data_buffer,
            is_static: interpreter.is_static,
            resume_at: 0,
        };
        (this, stack, stack_len)
    }

    /// Creates a new interpreter by cloning the context.
    pub fn to_interpreter(&self, stack: revm_interpreter::Stack) -> Interpreter {
        Interpreter {
            instruction_pointer: self.contract.bytecode.as_ptr(),
            contract: self.contract.clone(),
            instruction_result: InstructionResult::Continue,
            gas: *self.gas,
            shared_memory: self.memory.clone(),
            stack,
            return_data_buffer: self.return_data.to_vec().into(),
            is_static: self.is_static,
            next_action: self.next_action.clone(),
        }
    }
}

/// Extension trait for [`Host`].
pub trait HostExt: Host + Any {
    #[doc(hidden)]
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Host + Any> HostExt for T {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl dyn HostExt {
    /// Attempts to downcast the host to a concrete type.
    pub fn downcast_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.as_any_mut().downcast_mut()
    }
}

/// The raw function signature of a JIT'd EVM bytecode.
///
/// Prefer using [`JitEvmFn`] instead of this type. See [`JitEvmFn::call`] for more information.
// When changing the signature, also update the corresponding declarations in `fn translate`.
pub type RawJitEvmFn = unsafe extern "C" fn(
    gas: *mut Gas,
    stack: *mut EvmStack,
    stack_len: *mut usize,
    env: *mut Env,
    contract: *const Contract,
    ecx: *mut EvmContext<'_>,
) -> InstructionResult;

/// A JIT'd EVM bytecode function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitEvmFn(RawJitEvmFn);

impl JitEvmFn {
    /// Wraps the function.
    #[inline]
    pub const fn new(f: RawJitEvmFn) -> Self {
        Self(f)
    }

    /// Unwraps the function.
    #[inline]
    pub const fn into_inner(self) -> RawJitEvmFn {
        self.0
    }

    /// Calls the function by re-using the interpreter's resources.
    ///
    /// This behaves the same as [`Interpreter::run`], returning an [`InstructionResult`] in the
    /// interpreter's [`instruction_result`](Interpreter::instruction_result) field and the next
    /// action in the [`next_action`](Interpreter::next_action) field.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the function is safe to call.
    #[inline]
    pub unsafe fn call_with_interpreter(
        self,
        interpreter: &mut Interpreter,
        host: &mut dyn HostExt,
    ) -> InterpreterAction {
        let (mut ecx, stack, stack_len) =
            EvmContext::from_interpreter_with_stack(interpreter, host);
        interpreter.instruction_result = self.call(Some(stack), Some(stack_len), &mut ecx);
        if interpreter.next_action.is_some() {
            core::mem::take(&mut interpreter.next_action)
        } else {
            InterpreterAction::Return {
                result: InterpreterResult {
                    result: interpreter.instruction_result,
                    output: Bytes::new(),
                    gas: interpreter.gas,
                },
            }
        }
    }

    /// Calls the function.
    ///
    /// Arguments:
    /// - `stack`: Pointer to the stack. Must be `Some` if `local_stack` is set to `false`.
    /// - `stack_len`: Pointer to the stack length. Must be `Some` if `inspect_stack_length` is set
    ///   to `true`.
    /// - `ecx`: The context object.
    ///
    /// These conditions are enforced at runtime if `debug_assertions` is set to `true`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the arguments are valid and that the function is safe to call.
    #[inline]
    pub unsafe fn call(
        self,
        stack: Option<&mut EvmStack>,
        stack_len: Option<&mut usize>,
        ecx: &mut EvmContext<'_>,
    ) -> InstructionResult {
        (self.0)(
            ecx.gas,
            option_as_mut_ptr(stack),
            option_as_mut_ptr(stack_len),
            ecx.host.env_mut(),
            ecx.contract,
            ecx,
        )
    }
}

/// JIT EVM context stack.
#[repr(C)]
#[allow(missing_debug_implementations)]
pub struct EvmStack([MaybeUninit<EvmWord>; 1024]);

#[allow(clippy::new_without_default)]
impl EvmStack {
    /// The size of the stack in bytes.
    pub const SIZE: usize = 32 * Self::CAPACITY;

    /// The size of the stack in U256 elements.
    pub const CAPACITY: usize = 1024;

    /// Creates a new EVM stack, allocated on the stack.
    ///
    /// Use [`EvmStack::new_heap`] to create a stack on the heap.
    #[inline]
    pub fn new() -> Self {
        Self(unsafe { MaybeUninit::uninit().assume_init() })
    }

    /// Creates a vector that can be used as a stack.
    #[inline]
    #[cfg(feature = "alloc")]
    pub fn new_heap() -> Vec<EvmWord> {
        Vec::with_capacity(1024)
    }

    /// Creates a stack from the interpreter's stack. Assumes that the stack is large enough.
    #[inline]
    pub fn from_interpreter_stack(stack: &mut revm_interpreter::Stack) -> (&mut Self, &mut usize) {
        debug_assert!(stack.data().capacity() >= Self::CAPACITY);
        unsafe {
            let data = Self::from_mut_ptr(stack.data_mut().as_mut_ptr().cast());
            let len = &mut *stack.data_mut().as_mut_ptr().cast::<usize>().add(1);
            (data, len)
        }
    }

    /// Creates a stack from a vector's buffer.
    ///
    /// # Panics
    ///
    /// Panics if the vector's capacity is less than the required stack capacity.
    #[inline]
    #[cfg(feature = "std")]
    pub fn from_vec(vec: &Vec<EvmWord>) -> &Self {
        assert!(vec.capacity() >= Self::CAPACITY);
        unsafe { Self::from_ptr(vec.as_ptr()) }
    }

    /// Creates a stack from a mutable vector's buffer.
    ///
    /// The JIT'd function will overwrite the internal contents of the vector, and will not
    /// set the length. This is simply to have the stack allocated on the heap.
    ///
    /// # Panics
    ///
    /// Panics if the vector's capacity is less than the required stack capacity.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use revm_jit_context::EvmStack;
    /// let mut stack_buf = EvmStack::new_heap();
    /// let stack = EvmStack::from_mut_vec(&mut stack_buf);
    /// assert_eq!(stack.as_slice().len(), EvmStack::CAPACITY);
    /// ```
    #[inline]
    #[cfg(feature = "std")]
    pub fn from_mut_vec(vec: &mut Vec<EvmWord>) -> &mut Self {
        assert!(vec.capacity() >= Self::CAPACITY);
        unsafe { Self::from_mut_ptr(vec.as_mut_ptr()) }
    }

    /// Creates a stack from a slice.
    ///
    /// # Panics
    ///
    /// Panics if the slice's length is less than the required stack capacity.
    #[inline]
    pub const fn from_slice(slice: &[EvmWord]) -> &Self {
        assert!(slice.len() >= Self::CAPACITY);
        unsafe { Self::from_ptr(slice.as_ptr()) }
    }

    /// Creates a stack from a mutable slice.
    ///
    /// # Panics
    ///
    /// Panics if the slice's length is less than the required stack capacity.
    #[inline]
    pub fn from_mut_slice(slice: &mut [EvmWord]) -> &mut Self {
        assert!(slice.len() >= Self::CAPACITY);
        unsafe { Self::from_mut_ptr(slice.as_mut_ptr()) }
    }

    /// Creates a stack from a pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the pointer is valid and points to at least [`EvmStack::SIZE`]
    /// bytes.
    #[inline]
    pub const unsafe fn from_ptr<'a>(ptr: *const EvmWord) -> &'a Self {
        &*ptr.cast()
    }

    /// Creates a stack from a mutable pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the pointer is valid and points to at least [`EvmStack::SIZE`]
    /// bytes.
    #[inline]
    pub unsafe fn from_mut_ptr<'a>(ptr: *mut EvmWord) -> &'a mut Self {
        &mut *ptr.cast()
    }

    /// Returns the stack as a byte array.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; EvmStack::SIZE] {
        unsafe { &*self.0.as_ptr().cast() }
    }

    /// Returns the stack as a byte array.
    #[inline]
    pub fn as_bytes_mut(&mut self) -> &mut [u8; EvmStack::SIZE] {
        unsafe { &mut *self.0.as_mut_ptr().cast() }
    }

    /// Returns the stack as a slice.
    #[inline]
    pub const fn as_slice(&self) -> &[EvmWord; EvmStack::CAPACITY] {
        unsafe { &*self.0.as_ptr().cast() }
    }

    /// Returns the stack as a mutable slice.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [EvmWord; EvmStack::CAPACITY] {
        unsafe { &mut *self.0.as_mut_ptr().cast() }
    }
}

/// A native-endian 256-bit unsigned integer, aligned to 32 bytes.
///
/// This ends up being a simple no-op wrapper around `U256` on little-endian targets, modulo the
/// stricter alignment requirement, thanks to the `U256` representation being identical.
#[repr(C, align(32))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EvmWord([u8; 32]);

macro_rules! impl_fmt {
    ($($trait:ident),* $(,)?) => {
        $(
            impl fmt::$trait for EvmWord {
                #[inline]
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    self.to_u256().fmt(f)
                }
            }
        )*
    };
}

impl_fmt!(Debug, Display, Binary, Octal, LowerHex, UpperHex);

macro_rules! impl_conversions_through_u256 {
    ($($ty:ty),*) => {
        $(
            impl From<$ty> for EvmWord {
                #[inline]
                fn from(value: $ty) -> Self {
                    Self::from_u256(U256::from(value))
                }
            }

            impl From<&$ty> for EvmWord {
                #[inline]
                fn from(value: &$ty) -> Self {
                    Self::from(*value)
                }
            }

            impl From<&mut $ty> for EvmWord {
                #[inline]
                fn from(value: &mut $ty) -> Self {
                    Self::from(*value)
                }
            }

            impl TryFrom<EvmWord> for $ty {
                type Error = ();

                #[inline]
                fn try_from(value: EvmWord) -> Result<Self, Self::Error> {
                    value.to_u256().try_into().map_err(|_| ())
                }
            }

            impl TryFrom<&EvmWord> for $ty {
                type Error = ();

                #[inline]
                fn try_from(value: &EvmWord) -> Result<Self, Self::Error> {
                    (*value).try_into()
                }
            }

            impl TryFrom<&mut EvmWord> for $ty {
                type Error = ();

                #[inline]
                fn try_from(value: &mut EvmWord) -> Result<Self, Self::Error> {
                    (*value).try_into()
                }
            }
        )*
    };
}

impl_conversions_through_u256!(bool, u8, u16, u32, u64, usize, u128);

impl From<U256> for EvmWord {
    #[inline]
    fn from(value: U256) -> Self {
        Self::from_u256(value)
    }
}

impl From<&U256> for EvmWord {
    #[inline]
    fn from(value: &U256) -> Self {
        Self::from(*value)
    }
}

impl From<&mut U256> for EvmWord {
    #[inline]
    fn from(value: &mut U256) -> Self {
        Self::from(*value)
    }
}

impl EvmWord {
    /// The zero value.
    pub const ZERO: Self = Self([0; 32]);

    /// Creates a new value from native-endian bytes.
    #[inline]
    pub const fn from_ne_bytes(x: [u8; 32]) -> Self {
        Self(x)
    }

    /// Creates a new value from big-endian bytes.
    #[inline]
    pub fn from_be_bytes(x: [u8; 32]) -> Self {
        Self::from_be(Self(x))
    }

    /// Creates a new value from little-endian bytes.
    #[inline]
    pub fn from_le_bytes(x: [u8; 32]) -> Self {
        Self::from_le(Self(x))
    }

    /// Converts an integer from big endian to the target's endianness.
    #[inline]
    pub fn from_be(x: Self) -> Self {
        #[cfg(target_endian = "little")]
        return x.swap_bytes();
        #[cfg(target_endian = "big")]
        return x;
    }

    /// Converts an integer from little endian to the target's endianness.
    #[inline]
    pub fn from_le(x: Self) -> Self {
        #[cfg(target_endian = "little")]
        return x;
        #[cfg(target_endian = "big")]
        return x.swap_bytes();
    }

    /// Converts a [`U256`].
    #[inline]
    pub const fn from_u256(value: U256) -> Self {
        #[cfg(target_endian = "little")]
        return unsafe { core::mem::transmute(value) };
        #[cfg(target_endian = "big")]
        return Self(value.to_be_bytes());
    }

    /// Converts a [`U256`] reference to a [`U256`].
    #[inline]
    #[cfg(target_endian = "little")]
    pub const fn from_u256_ref(value: &U256) -> &Self {
        unsafe { &*(value as *const U256 as *const Self) }
    }

    /// Converts a [`U256`] mutable reference to a [`U256`].
    #[inline]
    #[cfg(target_endian = "little")]
    pub fn from_u256_mut(value: &mut U256) -> &mut Self {
        unsafe { &mut *(value as *mut U256 as *mut Self) }
    }

    /// Return the memory representation of this integer as a byte array in big-endian (network)
    /// byte order.
    #[inline]
    pub fn to_be_bytes(self) -> [u8; 32] {
        self.to_be().to_ne_bytes()
    }

    /// Return the memory representation of this integer as a byte array in little-endian byte
    /// order.
    #[inline]
    pub fn to_le_bytes(self) -> [u8; 32] {
        self.to_le().to_ne_bytes()
    }

    /// Return the memory representation of this integer as a byte array in native byte order.
    #[inline]
    pub const fn to_ne_bytes(self) -> [u8; 32] {
        self.0
    }

    /// Converts `self` to big endian from the target's endianness.
    #[inline]
    pub fn to_be(self) -> Self {
        #[cfg(target_endian = "little")]
        return self.swap_bytes();
        #[cfg(target_endian = "big")]
        return self;
    }

    /// Converts `self` to little endian from the target's endianness.
    #[inline]
    pub fn to_le(self) -> Self {
        #[cfg(target_endian = "little")]
        return self;
        #[cfg(target_endian = "big")]
        return self.swap_bytes();
    }

    /// Reverses the byte order of the integer.
    #[inline]
    pub fn swap_bytes(mut self) -> Self {
        self.0.reverse();
        self
    }

    /// Casts this value to a [`U256`]. This is a no-op on little-endian systems.
    #[cfg(target_endian = "little")]
    #[inline]
    pub const fn as_u256(&self) -> &U256 {
        unsafe { &*(self as *const Self as *const U256) }
    }

    /// Casts this value to a [`U256`]. This is a no-op on little-endian systems.
    #[cfg(target_endian = "little")]
    #[inline]
    pub fn as_u256_mut(&mut self) -> &mut U256 {
        unsafe { &mut *(self as *mut Self as *mut U256) }
    }

    /// Converts this value to a [`U256`]. This is a simple copy on little-endian systems.
    #[inline]
    pub const fn to_u256(&self) -> U256 {
        #[cfg(target_endian = "little")]
        return *self.as_u256();
        #[cfg(target_endian = "big")]
        return U256::from_be_bytes(self.0);
    }

    /// Converts this value to a [`U256`]. This is a no-op on little-endian systems.
    #[inline]
    pub const fn into_u256(self) -> U256 {
        #[cfg(target_endian = "little")]
        return unsafe { core::mem::transmute(self) };
        #[cfg(target_endian = "big")]
        return U256::from_be_bytes(self.0);
    }

    /// Converts this value to an [`Address`].
    #[inline]
    pub fn to_address(self) -> Address {
        Address::from_word(self.to_be_bytes().into())
    }
}

#[inline(always)]
fn option_as_mut_ptr<T>(opt: Option<&mut T>) -> *mut T {
    match opt {
        Some(ref_) => ref_,
        None => ptr::null_mut(),
    }
}
