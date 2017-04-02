use core::{ptr, slice};
use core::mem::{forget, size_of};
use std::fmt::{self, Display};
use std::error::Error;
use std::sync::Arc;
use super::Unpark;

/// Maxlmum size in bytes that will fit in a UnparkObject.
/// TODO: What should this value be?
/// We probably want to say that this value may increase but never decrease in a 1.x release.
const MAX_OBJ_BYTES : usize = 64;

/// Wrapper so we can implement 'Clone'.
#[derive(Copy)]
struct ByteBuffer([u8; MAX_OBJ_BYTES]);

impl Clone for ByteBuffer {
    fn clone(&self) -> Self {
        *self
    }
}

/// A VTable that knows how to clone because the data has a maximum size.
#[derive(Copy)]
struct UnparkVtable {
    unpark : fn(&[u8]),
    clone_to_byte_buffer : fn(&[u8]) -> ByteBuffer,
    drop_in_place : unsafe fn(&mut [u8]),
}

impl Clone for UnparkVtable {
    fn clone(&self) -> Self {
        Self { ..*self }
    }
}

impl UnparkVtable {
    fn new<T : Unpark + Clone>() -> UnparkVtable {
        UnparkVtable {
           unpark : Self::call_unpark::<T>,
           clone_to_byte_buffer : Self::clone_to_byte_buffer::<T>,
           drop_in_place : Self::drop_in_place::<T>,
       }
    }

    fn call_unpark<T : Unpark>(data : &[u8]) {
        let downcasted =  unsafe { &*(data as *const _ as *const T) };
        downcasted.unpark()
    }

    /// Returns array with bytes of clone.
    fn clone_to_byte_buffer<T : Clone>(data : &[u8]) -> ByteBuffer {
        let downcasted =  unsafe { &*(data as *const _ as *const T) };
        obliviate(downcasted.clone())
    }

    /// Make sure the value is forgotten to avoid double free if you call this.
    unsafe fn drop_in_place<T>(data : &mut [u8]) {
        ptr::drop_in_place(&mut *(data as *mut _ as *mut T));
    }
}

#[derive(Debug)]
// Holds size of type that triggered error.
pub struct UnparkTooLarge(usize);

impl Display for UnparkTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "The size of T is {} bytes which is more than the current limit of {} bytes.
                   If this is a problem for you please file an issue.", self.0, MAX_OBJ_BYTES)
    }
}

impl Error for UnparkTooLarge {
    fn description(&self) -> &str { "Type of 'unpark' too large" }
}

#[derive(Clone)]
enum Data<'a> {
    Borrowed(&'a [u8]),
    Owned(ByteBuffer)
}

impl<'a> Data<'a> {
    fn as_slice(&self) -> &[u8] {
        match *self {
            Data::Borrowed(data) => data,
            Data::Owned(ref data) => &data.0
        }
    }
}

/// This is used by methods like 'poll_future' which require an 'unpark' argument.
/// When 'park' is called, this handle may clone the 'unpark' directly
/// (if constructed with 'by_clone') or will use 'Arc' references (if constructed with 'boxed').
/// 'UnparkHandle' is very cheap to clone.
///
/// # Choosing between 'by_clone, 'try_by_clone' and 'boxed'
/// The difference between 'by_clone' and 'try_by_clone' is that 'by_clone' will fallback
/// to 'Arc' if the 'unpark' is too large and 'try_by_clone' will return an error.
/// If your 'unpark' is not 'Clone' then you must use 'boxed'.
/// If your 'unpark' is not 'Sync' or you are in 'no_std' then you must use 'try_by_clone'.
/// If 'unpark' is 'Copy' or is already an 'Arc' then the best choice is 'by_clone'.
/// When any constructor may be used, the only difference is in performance.
/// 'boxed' costs an allocation upfront and updates an atomic ref count on 'park',
/// while 'by_clone' has no upfront cost but will call 'unpark.clone()' on 'park'.
/// The best strategy depends on how often your futures 'park' and how costly 'unpark.clone()' is.
#[derive(Clone)]
#[allow(missing_debug_implementations)]
pub struct UnparkHandle<'a> {
    // This is a "lazy" UnparkObj, when cloning the data is necessary
    // to put it in a 'Task', it is cloned into a 'UnparkObj'.
    // 'data' will be 'Owned' if it's an 'Arc' constructed internally.
    data : Data<'a>,
    vtable : UnparkVtable,
}

impl<'a> Drop for UnparkHandle<'a> {
    fn drop(&mut self) {
        if let Data::Owned(mut data) = self.data {
            // We own 'data' and it was forgotten so this is safe.
            unsafe { (self.vtable.drop_in_place)(&mut data.0) };
        }
    }
}

impl<'a> UnparkHandle<'a> {
    /// 'try_by_clone' is the same as 'by_clone' but returns an error if the size of 'T' is too large.
    /// This can be used even if 'unpark' is not 'Send'.
    pub fn try_by_clone<T : Unpark + Clone>(unpark : &T) -> Result<UnparkHandle, UnparkTooLarge> {
        let size = size_of::<T>();
        if size <= MAX_OBJ_BYTES {
            let ptr = unpark as *const _ as *const u8;
            Ok(UnparkHandle {
                data : Data::Borrowed(unsafe { slice::from_raw_parts(ptr,  size_of::<T>()) }),
                vtable : UnparkVtable::new::<T>(),
            })
        } else {
            Err(UnparkTooLarge(size))
        }
    }

    /// Upon 'park' the 'unpark' argument will be cloned into the 'Task' handle returned.
    /// If the size of 'T' is larger than 64 bytes, 'by_clone' will fallback to using an 'Arc'.
    /// If 64 bytes is not enough for your use case, please report an issue.
    pub fn by_clone<T : Unpark + Clone + Sync>(unpark : &T) -> UnparkHandle {
        if let Ok(handle) = Self::try_by_clone(unpark) {
            handle
        } else { // Fallback to 'boxed' if necessary.
            Self::boxed(unpark.clone())
        }
    }

    /// Equivalent to 'let arc = Arc::new(unpark); UnparkHandle::by_clone(&arc)'.
    pub fn boxed<T : Unpark + Sync>(unpark : T) -> UnparkHandle<'static> {
        let arc = Arc::new(unpark);
        UnparkHandle {
            data : Data::Owned(obliviate(arc)),
            vtable : UnparkVtable::new::<Arc<T>>(),
        }
    }
}

impl<'a, T : Unpark + Sync> From<&'a Arc<T>> for UnparkHandle<'a> {
    fn from(unpark : &Arc<T>) -> UnparkHandle {
        Self::by_clone(unpark)
    }
}

/// A custom trait object that takes ownership of the data as a slice of bytes.
pub struct UnparkObj {
    data : ByteBuffer,
    vtable : UnparkVtable,
}

impl Drop for UnparkObj {
    fn drop(&mut self) {
        unsafe { (self.vtable.drop_in_place)(&mut self.data.0); }
    }
}

impl UnparkObj {
    fn new(data : &[u8], vtable : UnparkVtable) -> Self {
        UnparkObj {
            data : (vtable.clone_to_byte_buffer)(data),
            vtable : vtable,
        }
    }
}

impl Clone for UnparkObj {
    fn clone(&self) -> Self {
        Self::new(&((self.vtable.clone_to_byte_buffer)(&self.data.0)).0, self.vtable)
    }
}

impl<'a, 'b> From<&'a UnparkHandle<'b>> for UnparkObj {
    fn from(handle : &UnparkHandle) -> UnparkObj {
        UnparkObj::new(handle.data.as_slice(), handle.vtable)
    }
}

impl Unpark for UnparkObj {
    fn unpark(&self) {
        (self.vtable.unpark)(&self.data.0)
    }
}

/// Turns the victim into raw bytes and forgets it.
/// The caller now owns the value and is responsible for dropping it with 'drop_in_place<T>'.
fn obliviate<T>(victim : T) -> ByteBuffer {
    let size = size_of::<T>();
    assert!(size < MAX_OBJ_BYTES);
    let mut buffer = [0; MAX_OBJ_BYTES];
    // View victim and buffer as raw bytes.
    let victim_ptr = &victim as *const _ as *const u8;
    let buffer_ptr = &mut buffer as *mut _ as *mut u8;
    // Copy from 'victim' to 'buffer' and forget 'victim'.
    // Semantically, 'buffer' now owns 'victim'.
    unsafe { ptr::copy_nonoverlapping(victim_ptr, buffer_ptr, size); }
    forget(victim);
    ByteBuffer(buffer)
}
