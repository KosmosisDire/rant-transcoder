#include "transcoder.hpp"

#include "hosts.hpp"
#include "log.hpp"

#include <set>
#include <thread>

namespace transcoder {

using namespace std::chrono;

namespace {

rant::NodeOptions node_options(const Options& o) {
    rant::NodeOptions n;
    n.domain = o.domain;
    n.fetch_details = true;   /* schemas tell an Image from any other topic */
    n.max_topics = 512;       /* up to five per stream: two children, the parent, a descriptor */
    n.max_peers = 256;
    n.disable_logs = true;    /* it logs to stdout, and the log topics of a peer on an older Rant never match */
    return n;
}

/* What an entity of this kind and type gives a stream, false when nothing. */
bool origin_of(const rant::Entity& e, Origin& out) {
    if (!e.provides || e.schema.empty() || e.name.rfind("@rant/", 0) == 0) return false;
    std::string_view type = e.schema.name();
    if (e.kind == rant::EntityKind::Topic) {
        if (type == "Image") out = Origin::Image;
        else if (type == "VideoFrame") out = Origin::VideoFrame;
        else if (type == "ExternalVideoStream") out = Origin::StreamTopic;
        else return false;
        return true;
    }
    if (e.kind == rant::EntityKind::Variable && type == "ExternalVideoStream") {
        out = Origin::StreamVariable;
        return true;
    }
    return false;
}

}

Transcoder::Transcoder(const Options& options) : options_(options), node_(options.name, node_options(options)) {
    /* Rant repeats an error for as long as its cause lasts, so each line shows once a minute */
    node_.on_event([this](const rant::Event& e) {
        if (!e.is_error()) return;
        std::string text = e.to_string();
        auto now = steady_clock::now();
        std::lock_guard<std::mutex> g(told_mu_);
        if (told_.size() > 1000) told_.clear();
        auto it = told_.find(text);
        if (it != told_.end() && now - it->second < minutes(1)) return;
        told_[text] = now;
        say("rant: %s", text.c_str());
    });
}

void Transcoder::scan() {
    rant::Reflection r = node_.reflection();
    std::set<std::string> hosts = host_addresses();
    std::map<std::string, Parent> found;
    for (const rant::Peer& p : r.peers()) {
        if (!p.active || p.name == options_.name || !hosts.count(address_ip(p.address))) continue;
        for (const rant::Entity& e : r.entities(p.id)) {
            Parent parent{ e.name, Origin::Image, p.id };
            if (origin_of(e, parent.origin)) found.emplace(e.name, parent);   /* the first local publisher wins */
        }
    }
    for (auto it = streams_.begin(); it != streams_.end();) {
        auto f = found.find(it->first);
        if (f != found.end() && f->second == it->second->parent()) {
            ++it;
            continue;
        }
        say("%s: gone", it->first.c_str());
        it = streams_.erase(it);
    }
    for (auto& [name, parent] : found) {
        if (streams_.count(name)) continue;
        say("%s: found on this host", name.c_str());
        streams_.emplace(name, std::make_unique<Stream>(node_, parent, options_.encoder));
    }
}

void Transcoder::run(const std::function<bool()>& stop) {
    auto next_scan = steady_clock::now();
    while (!stop()) {
        auto now = steady_clock::now();
        uint32_t epoch = node_.reflection().epoch();
        /* the epoch moves on every change, so a busy mesh still scans at most twice a second */
        if ((epoch != epoch_ && now >= next_scan - milliseconds(1500)) || now >= next_scan) {
            epoch_ = epoch;
            scan();
            next_scan = now + seconds(2);
        }
        for (auto& s : streams_) s.second->tick();
        std::this_thread::sleep_for(milliseconds(100));
    }
    streams_.clear();
}

}
