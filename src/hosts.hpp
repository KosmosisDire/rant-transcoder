#pragma once

#include <set>
#include <string>

namespace transcoder {

/* This host's IPv4 addresses, loopback included. A peer is local when the IP of its
 * announced address is one of them. */
std::set<std::string> host_addresses();

/* The IP of an "ip:port" address. */
std::string address_ip(const std::string& address);

}
