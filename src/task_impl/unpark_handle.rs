use core::{ptr, slice};
use core::mem::{forget, size_of, transmute};
use std::fmt::{self, Debug, Display};
use std::error::Error;
use std::sync::Arc;
use super::Unpark;

/// Maxlmum size in bytes that will fit in a UnparkObject.
/// TODO: What should this value be?
/// We probably want to say that this value may increase but never decrease in a 1.x release.
const MAX_OBJ_BYTES : usize = 128;

// A VTable that knows how to clone because the data has a maximum size.
#[derive(Copy)]
struct UnparkVtable {
    unpark : fn(&[u8]),
    clone_as_array : fn(&[u8]) -> [u8; MAX_OBJ_BYTES],
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
           clone_as_array : Self::clone_as_array::<T>,
           drop_in_place : Self::drop_in_place::<T>,
       }
    }

    fn call_unpark<T : Unpark>(obj : &[u8]) {
        let x =  unsafe { &*(obj as *const _ as *const T) };
        x.unpark()
    }

    /// Returns array with bytes of clone.
    fn clone_as_array<T : Clone>(obj : &[u8]) -> [u8; MAX_OBJ_BYTES] {
        let x =  unsafe { &*(obj as *const _ as *const T) };
        let cloned = x.clone();
        let mut buffer = [0; MAX_OBJ_BYTES];
        // View cloned and buffer as raw bytes.
        let cloned_ptr = &cloned as *const _ as *const u8;
        let buffer_ptr = &mut buffer as *mut _ as *mut u8;
        // Copy from cloned to the buffer and forget cloned.
        // Semantically, the buffer now owns cloned.
        unsafe { ptr::copy_nonoverlapping(cloned_ptr, buffer_ptr, size_of::<T>()); }
        forget(cloned);
        buffer
    }

    /// Make sure the value is forgotten to avoid double free if you call this.
    unsafe fn drop_in_place<T>(obj : &mut [u8]) {
        ptr::drop_in_place(&mut *(obj as *mut _ as *mut T));
    }
}

#[derive(Debug)]
// Holds size of type that triggered error.
pub struct UnparkTooLarge(usize);

impl fmt::Display for UnparkTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "The size of T is {} bytes which is more than the current limit of {} bytes.
                   If this is a problem for you please file an issue.", self.0, MAX_OBJ_BYTES)
    }
}

/// This is used by methods like 'poll_future' which require an 'unpark' argument.
/// When 'park' is called, this handle may clone the 'unpark' directly
/// (if constructed with 'by_clone') or will use 'Arc' references (if constructed with 'boxed').
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
pub struct UnparkHandle<'a> {
    // A custom trait object that can clone it's data,
    // carries the necessary information to make a `UnparkObj`.
    data : &'a [u8],
    vtable : UnparkVtable,
    owns_data : bool // Are we responsible for dropping the data?
}

impl<'a> Drop for UnparkHandle<'a> {
    fn drop(&mut self) {
        if self.owns_data {
            // We own data so the transmute is safe.
            (self.vtable.drop_in_place)(unsafe { transmute::<&_, &mut _>(self.data) });
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
                data : unsafe { slice::from_raw_parts(ptr,  size_of::<T>()) },
                vtable : UnparkVtable::new::<T>(),
                owns_data : false
            })
        } else {
            Err(UnparkTooLarge(size))
        }
    }

    /// Upon 'park' the 'unpark' argument will be cloned into the 'Task' handle returned.
    /// If the size of 'T' is larger than 128 bytes, 'by_clone' will fallback to using an 'Arc'.
    /// If 128 bytes is not enough for your use case, please report an issue.
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
        let ptr = &arc as *const _ as *const u8;
        forget(arc); // 'arc' will be owned by the handle.
        UnparkHandle {
            data : unsafe { slice::from_raw_parts(ptr,  size_of::<Arc<T>>()) },
            vtable : UnparkVtable::new::<Arc<T>>(),
            owns_data : true
        }
    }
}

impl<'a, T : Unpark + Sync> From<&'a Arc<T>> for UnparkHandle<'a> {
    fn from(unpark : &Arc<T>) -> UnparkHandle {
        Self::by_clone(unpark)
    }
}

/// A custom trait object that takes ownership of the data as a slice of bytes.
/// Semantically 'Copy'.
pub struct UnparkObj {
    data : [u8; MAX_OBJ_BYTES],
    vtable : UnparkVtable,
}

impl Drop for UnparkObj {
    fn drop(&mut self) {
        (self.vtable.drop_in_place)(&mut self.data);
    }
}

impl Clone for UnparkObj {
    /// Just a copy.
    fn clone(&self) -> Self {
        Self { ..*self }
    }
}

impl<'a, 'b> From<&'a UnparkHandle<'b>> for UnparkObj {
    fn from(handle :  &UnparkHandle) -> UnparkObj {
        UnparkObj {
            data : (handle.vtable.clone_as_array)(handle.data),
            vtable : handle.vtable,
        }
    }
}

impl Unpark for UnparkObj {
    fn unpark(&self) {
        (self.vtable.unpark)(&self.data)
    }
}
