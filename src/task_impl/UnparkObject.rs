const MAX_OBJ_BYTES : usize = 64;
/// A custom trait object that takes ownership of the object as a slice of bytes.
struct UnparkObj {
    obj : [u8; MAX_OBJ_BYTES],
    unpark_obj : fn(&[u8]),
    drop_obj : fn(&mut [u8]),
}

impl Unpark for UnparkObj {
    fn unpark(&self) {
        (self.unpark_obj)(self.obj)
    }
}
/// Trait for dropping manually. `forget` after calling.
trait ForceDrop {
    fn force_drop(&mut self) {
       unsafe { ptr::drop_in_place(self) }
    }
}
impl<T> ForceDrop for T {}

fn call_unpark<T : Unpark>(obj : &[u8]) {
    let x =  unsafe { &*(obj as *const _ as *const T) };
    x.unpark()
}

// Returns array with bytes of clone.
fn call_clone<T : Clone>(obj : &[u8]) -> [u8; MAX_OBJ_BYTES] {
    let x =  unsafe { &*(obj as *const _ as *const T) };
    let cloned = x.clone();
    let mut buffer = [0; MAX_OBJ_BYTES];
    // View cloned and buffer as raw bytes.
    let cloned_ptr = &cloned as *const _ as *const u8;
    let buffer_ptr = &mut buffer as *mut _ as *mut u8;
    // Copy from cloned to the buffer and forget cloned.
    // Semantically, the buffer now owns cloned.
    unsafe { ptr::copy_nonoverlapping(cloned_ptr, buffer_ptr, mem::size_of::<T>()); }
    mem::forget(cloned);
    buffer
}

fn call_force_drop<T>(obj : &mut [u8]) {
    let x =  unsafe { &mut *(obj as *mut _ as *mut T) };
    x.force_drop()
}
