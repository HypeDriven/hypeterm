//go:build linux || android

package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"encoding/json"
	"runtime"
)

// isAndroid decides whether the getifaddrs-based enumeration replaces the standard
// library's. Kept as a variable so a host test can exercise the Android path.
var isAndroid = runtime.GOOS == "android"

//export hypeterm_tsnet_interfaces
//
// Writes the interfaces as seen by the enumeration the node uses, as a JSON array of
// {name, index, mtu, flags, addresses}. Same buffer convention as status.
//
// It exists so the replacement for Go's blocked net.Interfaces() is observable: on the
// host it can be compared against the standard library, and on a device it answers
// "does this phone let us see its interfaces at all" without a debugger. It reports no
// hardware addresses — Android hides them, and nothing here needs them.
func hypeterm_tsnet_interfaces(buf *C.char, length C.int) C.int {
	interfaces, err := systemInterfaces()
	if err != nil {
		setError(err)
		return -1
	}
	type entry struct {
		Name      string   `json:"name"`
		Index     int      `json:"index"`
		MTU       int      `json:"mtu"`
		Up        bool     `json:"up"`
		Loopback  bool     `json:"loopback"`
		Addresses []string `json:"addresses"`
	}
	report := make([]entry, 0, len(interfaces))
	for _, iface := range interfaces {
		item := entry{
			Name:      iface.Name,
			Index:     iface.Index,
			MTU:       iface.MTU,
			Up:        iface.IsUp(),
			Loopback:  iface.IsLoopback(),
			Addresses: []string{},
		}
		addresses, _ := iface.Addrs()
		for _, address := range addresses {
			item.Addresses = append(item.Addresses, address.String())
		}
		report = append(report, item)
	}
	encoded, err := json.Marshal(report)
	if err != nil {
		setError(err)
		return -1
	}
	return copyOut(string(encoded), buf, length)
}
