#include "hosts.hpp"

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <winsock2.h>
#include <ws2tcpip.h>
#include <iphlpapi.h>
#include <vector>
#else
#include <arpa/inet.h>
#include <ifaddrs.h>
#include <netinet/in.h>
#endif

namespace transcoder {

std::set<std::string> host_addresses() {
    std::set<std::string> out{ "127.0.0.1" };
    char text[INET_ADDRSTRLEN];
#ifdef _WIN32
    ULONG size = 16 * 1024;
    std::vector<unsigned char> buf;
    ULONG r = ERROR_BUFFER_OVERFLOW;
    for (int tries = 0; tries < 3 && r == ERROR_BUFFER_OVERFLOW; tries++) {
        buf.resize(size);
        r = GetAdaptersAddresses(AF_INET, GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                                 nullptr, reinterpret_cast<IP_ADAPTER_ADDRESSES*>(buf.data()), &size);
    }
    if (r != NO_ERROR) return out;
    for (auto* a = reinterpret_cast<IP_ADAPTER_ADDRESSES*>(buf.data()); a; a = a->Next)
        for (auto* u = a->FirstUnicastAddress; u; u = u->Next) {
            auto* in = reinterpret_cast<sockaddr_in*>(u->Address.lpSockaddr);
            if (inet_ntop(AF_INET, &in->sin_addr, text, sizeof text)) out.insert(text);
        }
#else
    ifaddrs* list = nullptr;
    if (getifaddrs(&list) != 0) return out;
    for (ifaddrs* a = list; a; a = a->ifa_next) {
        if (!a->ifa_addr || a->ifa_addr->sa_family != AF_INET) continue;
        auto* in = reinterpret_cast<sockaddr_in*>(a->ifa_addr);
        if (inet_ntop(AF_INET, &in->sin_addr, text, sizeof text)) out.insert(text);
    }
    freeifaddrs(list);
#endif
    return out;
}

std::string address_ip(const std::string& address) {
    size_t colon = address.rfind(':');
    return colon == std::string::npos ? address : address.substr(0, colon);
}

}
