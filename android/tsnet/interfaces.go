//go:build linux || android

package main

// Interface enumeration that works on Android.
//
// Go's net.Interfaces() asks the kernel over NETLINK_ROUTE with RTM_GETLINK, which
// Android refuses to answer for ordinary applications — it would expose hardware
// addresses of nearby networks. Tailscale hits this the moment a node starts
// (tailscale/tailscale#2293) and fails with "route ip+net: netlinkrib: permission
// denied", so it provides a hook for the host to answer the question another way.
//
// libc's getifaddrs(3) is that other way: bionic implements it through a path apps are
// allowed to use, and it returns names, indices, flags and addresses — everything the
// network monitor needs. The official Android client routes this through Java instead;
// going straight to libc keeps it in one language and one process.

/*
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

// ifr_mtu is a macro over a union, which cgo cannot reach; ask C for the number.
static int hypeterm_iface_mtu(const char* name) {
  int fd = socket(AF_INET, SOCK_DGRAM, 0);
  if (fd < 0) return 0;
  struct ifreq request;
  memset(&request, 0, sizeof(request));
  strncpy(request.ifr_name, name, IFNAMSIZ - 1);
  int mtu = 0;
  if (ioctl(fd, SIOCGIFMTU, &request) == 0) mtu = request.ifr_mtu;
  close(fd);
  return mtu;
}
*/
import "C"

import (
	"errors"
	"net"
	"unsafe"

	"tailscale.com/net/netmon"
)

// systemInterfaces reports the machine's interfaces and their addresses without
// touching netlink.
func systemInterfaces() ([]netmon.Interface, error) {
	var list *C.struct_ifaddrs
	if C.getifaddrs(&list) != 0 {
		return nil, errors.New("getifaddrs failed")
	}
	defer C.freeifaddrs(list)

	// One entry per interface, with every address on it collected into AltAddrs —
	// getifaddrs reports one record per address, not per interface.
	byName := map[string]*netmon.Interface{}
	var order []string

	for entry := list; entry != nil; entry = entry.ifa_next {
		if entry.ifa_name == nil {
			continue
		}
		name := C.GoString(entry.ifa_name)
		iface, seen := byName[name]
		if !seen {
			iface = &netmon.Interface{
				Interface: &net.Interface{
					Index: int(C.if_nametoindex(entry.ifa_name)),
					Name:  name,
					MTU:   int(C.hypeterm_iface_mtu(entry.ifa_name)),
					Flags: goFlags(uint32(entry.ifa_flags)),
				},
				// Non-nil even when empty: a nil AltAddrs sends netmon back to
				// net.Interface.Addrs(), which is the call that is denied.
				AltAddrs: []net.Addr{},
			}
			byName[name] = iface
			order = append(order, name)
		}
		if addr := toIPNet(entry.ifa_addr, entry.ifa_netmask); addr != nil {
			iface.AltAddrs = append(iface.AltAddrs, addr)
		}
	}

	result := make([]netmon.Interface, 0, len(order))
	for _, name := range order {
		result = append(result, *byName[name])
	}
	return result, nil
}

// goFlags converts the kernel's IFF_* bits to net.Flags, which are not the same values.
func goFlags(raw uint32) net.Flags {
	var flags net.Flags
	if raw&C.IFF_UP != 0 {
		flags |= net.FlagUp
	}
	if raw&C.IFF_RUNNING != 0 {
		flags |= net.FlagRunning
	}
	if raw&C.IFF_BROADCAST != 0 {
		flags |= net.FlagBroadcast
	}
	if raw&C.IFF_LOOPBACK != 0 {
		flags |= net.FlagLoopback
	}
	if raw&C.IFF_POINTOPOINT != 0 {
		flags |= net.FlagPointToPoint
	}
	if raw&C.IFF_MULTICAST != 0 {
		flags |= net.FlagMulticast
	}
	return flags
}

func toIPNet(addr, netmask *C.struct_sockaddr) net.Addr {
	if addr == nil {
		return nil
	}
	switch addr.sa_family {
	case C.AF_INET:
		in := (*C.struct_sockaddr_in)(unsafe.Pointer(addr))
		ip := make(net.IP, net.IPv4len)
		copy(ip, (*[net.IPv4len]byte)(unsafe.Pointer(&in.sin_addr))[:])
		return &net.IPNet{IP: ip, Mask: maskOf(netmask, net.IPv4len)}
	case C.AF_INET6:
		in := (*C.struct_sockaddr_in6)(unsafe.Pointer(addr))
		ip := make(net.IP, net.IPv6len)
		copy(ip, (*[net.IPv6len]byte)(unsafe.Pointer(&in.sin6_addr))[:])
		return &net.IPNet{IP: ip, Mask: maskOf(netmask, net.IPv6len)}
	default:
		// AF_PACKET and friends carry no IP; the interface itself is already recorded.
		return nil
	}
}

// maskOf reads the netmask sockaddr, falling back to a host route when the kernel did
// not supply one — a wrong prefix would misreport the link's subnet, a /32 only says
// "this address".
func maskOf(netmask *C.struct_sockaddr, length int) net.IPMask {
	full := net.CIDRMask(length*8, length*8)
	if netmask == nil {
		return full
	}
	switch {
	case length == net.IPv4len && netmask.sa_family == C.AF_INET:
		in := (*C.struct_sockaddr_in)(unsafe.Pointer(netmask))
		mask := make(net.IPMask, net.IPv4len)
		copy(mask, (*[net.IPv4len]byte)(unsafe.Pointer(&in.sin_addr))[:])
		return mask
	case length == net.IPv6len && netmask.sa_family == C.AF_INET6:
		in := (*C.struct_sockaddr_in6)(unsafe.Pointer(netmask))
		mask := make(net.IPMask, net.IPv6len)
		copy(mask, (*[net.IPv6len]byte)(unsafe.Pointer(&in.sin6_addr))[:])
		return mask
	default:
		return full
	}
}

func init() {
	// Only Android needs it. Everywhere else the standard library is authoritative and
	// knows things getifaddrs does not, so leave it alone — but the implementation is
	// still compiled and tested on the host through hypeterm_tsnet_interfaces.
	if isAndroid {
		netmon.RegisterInterfaceGetter(systemInterfaces)
	}
}
