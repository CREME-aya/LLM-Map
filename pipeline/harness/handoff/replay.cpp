#include <algorithm>
#include <array>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <cstdint>
#include <iostream>
#include <stdexcept>
#include <string>

// 1 回の Actor 応答を置き換えるホスト検体。Trip の有向辺順序と Directions の leg を保持する。
struct Traversal {
    uint32_t index;
    bool forward;
    int start_ppm;
    int end_ppm;
};

static unsigned le16(const unsigned char* buf, size_t at) {
    return buf[at] | (unsigned(buf[at+1]) << 8);
}

static void emit_edge(const Traversal& edge, const unsigned char* tile) {
    const auto at = 52 + 4 * edge.index;
    const auto flags = tile[at+3];
    std::cout << "{\"edge_id\":\"" << (uint64_t(edge.index) << 25) << "\",\"direction\":\""
              << (edge.forward ? "forward" : "reverse") << "\",\"start_ppm\":" << edge.start_ppm
              << ",\"end_ppm\":" << edge.end_ppm << ",\"length_cm\":20000,\"two_way\":true,"
              << "\"residential\":true,\"width_cm\":";
    if (flags & 2) {
        std::cout << "{\"kind\":\"confirmed\",\"value\":" << le16(tile, at)
                  << ",\"source_ref\":\"sidecar:0/" << edge.index << "\"}";
    } else {
        std::cout << "{\"kind\":\"unknown\",\"reason\":\"missing\"}";
    }
    std::cout << "}";
}

int main(int argc, char** argv) {
    if (argc != 2) return 2;
    const int fd = open(argv[1], O_RDONLY);
    if (fd < 0) return 2;
    struct stat info{};
    if (fstat(fd, &info) != 0 || info.st_size != 60) {
        close(fd);
        return 2;
    }
    void* mapped = mmap(nullptr, 60, PROT_READ, MAP_PRIVATE, fd, 0);
    close(fd);
    if (mapped == MAP_FAILED) return 2;
    const auto* tile = static_cast<unsigned char*>(mapped);
    // PoC の固定探索にピン留めした graph_version と GraphTile の有向辺数を照合する。
    const std::array<unsigned char, 32> version = {0xbb, 0x35, 0x71, 0x15, 0x6f, 0x71, 0xc8, 0x87, 0x56, 0xa5, 0xa0, 0xb5, 0x2f, 0xe7, 0xc0, 0x2c, 0xc4, 0x31, 0x50, 0xc5, 0xc9, 0x53, 0xf9, 0xaa, 0xf, 0xd8, 0x8b, 0xa1, 0x9e, 0xa, 0xba, 0x75};
    bool valid = std::string(reinterpret_cast<const char*>(tile), 4) == "DSC1"
        && le16(tile, 4) == 1 && le16(tile, 6) == 4
        && std::all_of(tile+8, tile+16, [](auto byte){ return byte == 0; })
        && std::equal(version.begin(), version.end(), tile+16)
        && le16(tile, 48) == 2 && le16(tile, 50) == 0;
    for (size_t at : {52, 56}) {
        const unsigned flags = tile[at+3];
        valid = valid && flags < 16 && bool(flags & 2) == bool(le16(tile, at))
            && bool(flags & 4) == bool(tile[at+2]);
    }
    if (!valid) {
        munmap(mapped, 60);
        return 2;
    }
    std::cout << "{ \"schema_version\":1, \"plan_request_id\":\"計画-1\","
              << "\"graph_version\":\"jp-test-1\",\"candidates\":[";
    for (int candidate=0; candidate<7; ++candidate) {
        if (candidate) std::cout << ",";
        std::cout << "{\"candidate_id\":\"c" << candidate << "\",\"label\":\""
                  << (candidate == 0 ? "baseline" : "costing_variant")
                  << "\",\"legal\":true,\"complete\":true,\"eta_ms\":" << 240000+candidate*1000
                  << ",\"legs\":[[";
        emit_edge({0, true, candidate*50000, 750000}, tile);
        std::cout << ",";
        emit_edge({1, false, 250000, 1000000}, tile);
        std::cout << "]],\"maneuvers\":[{\"kind\":\"crossing_right_signal_unknown\",\"leg\":0,\"incoming\":0,\"reason\":\"missing\"}]}";
    }
    std::cout << "]}";
    munmap(mapped, 60);
}
