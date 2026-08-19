package main

/*
#include <stdlib.h>
*/
import "C"

import "unsafe"

// unsafeSlice views a caller-provided buffer as a Go slice. The slice never outlives
// the call that created it, so the C memory is not held across the cgo boundary.
func unsafeSlice(buf *C.char, length int) []byte {
	return unsafe.Slice((*byte)(unsafe.Pointer(buf)), length)
}
