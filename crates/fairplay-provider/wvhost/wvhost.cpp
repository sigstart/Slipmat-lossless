// SPDX-FileCopyrightText: 2026 Jan Vermeulen
// SPDX-License-Identifier: GPL-3.0-or-later

// wvhost: hosts Google's Widevine CDM for the FairPlay provider, which starts
// it inside its own sandbox and speaks to it over pipes. It is a Chromium CDM
// host (interface 10), as a browser is, and does nothing a browser would not:
// the CDM keeps its keys, and samples go in encrypted and come out clear.
//
// One line per message, binary fields base64:
//   in   CERT <cert>                     the licence server's certificate
//        PSSH <box>                      open a temporary session from a cenc PSSH
//        UPDATE <licence>                the licence server's answer
//        DECRYPT <kid> <iv> <map> <data> map is clear:cipher,... or - for none
//        QUIT
//   out  READY | INITFAIL
//        MSG <type> <message>            for the licence server; 0 is a request
//        KEYS <kid hex>:<status> ...     ids and statuses, never keys
//        PROMISE <id> ok | PROMISE <id> reject <exception> <code> <text>
//        CLEAR <status> <data>           answers DECRYPT, in order; 0 is success
//        LOG <text>
//
// Usage: wvhost <libwidevinecdm.so> <state directory>

#include <dlfcn.h>
#include <poll.h>
#include <sys/random.h>
#include <unistd.h>

#include <algorithm>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <deque>
#include <fstream>
#include <functional>
#include <iterator>
#include <mutex>
#include <sstream>
#include <string>
#include <vector>

#include "content_decryption_module.h"

namespace {

/// A line longer than this is not one the provider sends.
constexpr size_t kMaxLine = 4 << 20;

std::mutex g_out;
void emit(const std::string &line) {
    std::lock_guard<std::mutex> lock(g_out);
    std::fwrite(line.data(), 1, line.size(), stdout);
    std::fputc('\n', stdout);
    std::fflush(stdout);
}

const char kB64[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

std::string b64encode(const uint8_t *data, size_t len) {
    std::string out;
    out.reserve((len + 2) / 3 * 4);
    for (size_t i = 0; i < len; i += 3) {
        uint32_t v = uint32_t(data[i]) << 16;
        if (i + 1 < len) v |= uint32_t(data[i + 1]) << 8;
        if (i + 2 < len) v |= data[i + 2];
        out += kB64[(v >> 18) & 63];
        out += kB64[(v >> 12) & 63];
        out += i + 1 < len ? kB64[(v >> 6) & 63] : '=';
        out += i + 2 < len ? kB64[v & 63] : '=';
    }
    return out;
}

/// False on anything outside the alphabet.
bool b64decode(const std::string &in, std::vector<uint8_t> &out) {
    out.clear();
    uint32_t v = 0;
    int bits = 0;
    for (char c : in) {
        if (c == '=') break;
        const char *p = c ? std::strchr(kB64, c) : nullptr;
        if (!p) return false;
        v = (v << 6) | uint32_t(p - kB64);
        bits += 6;
        if (bits >= 8) {
            bits -= 8;
            out.push_back(uint8_t(v >> bits));
        }
    }
    return true;
}

std::string hex(const uint8_t *data, size_t len) {
    static const char digits[] = "0123456789abcdef";
    std::string out;
    for (size_t i = 0; i < len; i++) {
        out += digits[data[i] >> 4];
        out += digits[data[i] & 15];
    }
    return out;
}

// Callbacks the CDM expects later, as a browser delivers them, never from
// inside the CDM's own call.
std::deque<std::function<void()>> g_tasks;

struct Timer {
    std::chrono::steady_clock::time_point due;
    void *context;
};
std::vector<Timer> g_timers;

class HostBuffer : public cdm::Buffer {
  public:
    explicit HostBuffer(uint32_t capacity) : data_(capacity), size_(0) {}
    void Destroy() override { delete this; }
    uint32_t Capacity() const override { return uint32_t(data_.size()); }
    uint8_t *Data() override { return data_.data(); }
    void SetSize(uint32_t size) override { size_ = std::min<uint32_t>(size, Capacity()); }
    uint32_t Size() const override { return size_; }

  private:
    std::vector<uint8_t> data_;
    uint32_t size_;
};

class Block : public cdm::DecryptedBlock {
  public:
    ~Block() override {
        if (buffer_) buffer_->Destroy();
    }
    void SetDecryptedBuffer(cdm::Buffer *buffer) override { buffer_ = buffer; }
    cdm::Buffer *DecryptedBuffer() override { return buffer_; }
    void SetTimestamp(int64_t timestamp) override { timestamp_ = timestamp; }
    int64_t Timestamp() const override { return timestamp_; }

  private:
    cdm::Buffer *buffer_ = nullptr;
    int64_t timestamp_ = 0;
};

/// The CDM's storage, as files in the state directory. Names are the CDM's
/// own and never contain a path.
class HostFileIO : public cdm::FileIO {
  public:
    HostFileIO(cdm::FileIOClient *client, std::string dir) : client_(client), dir_(std::move(dir)) {}
    void Open(const char *name, uint32_t size) override {
        std::string file(name, size);
        bool safe = !file.empty() && file.find('/') == std::string::npos && file[0] != '.';
        path_ = dir_ + "/" + file;
        auto status = safe ? cdm::FileIOClient::Status::kSuccess : cdm::FileIOClient::Status::kError;
        g_tasks.push_back([this, status] { client_->OnOpenComplete(status); });
    }
    void Read() override {
        g_tasks.push_back([this] {
            std::ifstream in(path_, std::ios::binary);
            std::vector<uint8_t> bytes((std::istreambuf_iterator<char>(in)), {});
            client_->OnReadComplete(cdm::FileIOClient::Status::kSuccess, bytes.data(), uint32_t(bytes.size()));
        });
    }
    void Write(const uint8_t *data, uint32_t size) override {
        std::vector<uint8_t> bytes(data, data + size);
        g_tasks.push_back([this, bytes] {
            std::ofstream out(path_, std::ios::binary | std::ios::trunc);
            out.write(reinterpret_cast<const char *>(bytes.data()), std::streamsize(bytes.size()));
            client_->OnWriteComplete(out ? cdm::FileIOClient::Status::kSuccess
                                         : cdm::FileIOClient::Status::kError);
        });
    }
    void Close() override { delete this; }

  private:
    cdm::FileIOClient *client_;
    std::string dir_;
    std::string path_;
};

cdm::ContentDecryptionModule_10 *g_cdm = nullptr;
std::string g_state;
std::string g_session;

class Host : public cdm::Host_10 {
  public:
    cdm::Buffer *Allocate(uint32_t capacity) override { return new HostBuffer(capacity); }
    void SetTimer(int64_t delay_ms, void *context) override {
        g_timers.push_back({std::chrono::steady_clock::now() + std::chrono::milliseconds(delay_ms), context});
    }
    cdm::Time GetCurrentWallTime() override {
        using namespace std::chrono;
        return duration<double>(system_clock::now().time_since_epoch()).count();
    }
    void OnInitialized(bool success) override { emit(success ? "READY" : "INITFAIL"); }
    void OnResolveKeyStatusPromise(uint32_t id, cdm::KeyStatus status) override {
        emit("PROMISE " + std::to_string(id) + " ok status=" + std::to_string(status));
    }
    void OnResolveNewSessionPromise(uint32_t id, const char *session, uint32_t size) override {
        g_session.assign(session, size);
        emit("PROMISE " + std::to_string(id) + " ok");
    }
    void OnResolvePromise(uint32_t id) override { emit("PROMISE " + std::to_string(id) + " ok"); }
    void OnRejectPromise(uint32_t id, cdm::Exception exception, uint32_t code, const char *text,
                         uint32_t size) override {
        std::string message(text ? text : "", text ? size : 0);
        std::replace(message.begin(), message.end(), '\n', ' ');
        emit("PROMISE " + std::to_string(id) + " reject " + std::to_string(exception) + " " +
             std::to_string(code) + " " + message);
    }
    void OnSessionMessage(const char *, uint32_t, cdm::MessageType type, const char *message,
                          uint32_t size) override {
        emit("MSG " + std::to_string(type) + " " + b64encode(reinterpret_cast<const uint8_t *>(message), size));
    }
    void OnSessionKeysChange(const char *, uint32_t, bool, const cdm::KeyInformation *keys,
                             uint32_t count) override {
        std::string line = "KEYS";
        for (uint32_t i = 0; i < count; i++)
            line += " " + hex(keys[i].key_id, keys[i].key_id_size) + ":" + std::to_string(keys[i].status);
        emit(line);
    }
    void OnExpirationChange(const char *, uint32_t, cdm::Time) override {}
    void OnSessionClosed(const char *, uint32_t) override { emit("LOG session closed"); }
    void SendPlatformChallenge(const char *, uint32_t, const char *, uint32_t) override {
        // Desktop Linux has no platform verification; say so, as Chrome does.
        g_tasks.push_back([] {
            cdm::PlatformChallengeResponse empty{};
            g_cdm->OnPlatformChallengeResponse(empty);
        });
    }
    void EnableOutputProtection(uint32_t) override {}
    void QueryOutputProtectionStatus() override {
        g_tasks.push_back([] { g_cdm->OnQueryOutputProtectionStatus(cdm::kQuerySucceeded, 0, 0); });
    }
    void OnDeferredInitializationDone(cdm::StreamType, cdm::Status) override {}
    cdm::FileIO *CreateFileIO(cdm::FileIOClient *client) override { return new HostFileIO(client, g_state); }
    void RequestStorageId(uint32_t version) override {
        g_tasks.push_back([version] {
            std::string path = g_state + "/storage_id";
            std::vector<uint8_t> id(32);
            std::ifstream in(path, std::ios::binary);
            if (!in.read(reinterpret_cast<char *>(id.data()), 32)) {
                if (getrandom(id.data(), id.size(), 0) != ssize_t(id.size())) id.assign(32, 0);
                std::ofstream(path, std::ios::binary).write(reinterpret_cast<char *>(id.data()), 32);
            }
            g_cdm->OnStorageId(version, id.data(), uint32_t(id.size()));
        });
    }
};

Host g_host;

void *get_host(int version, void *) { return version == cdm::Host_10::kVersion ? &g_host : nullptr; }

void run_due() {
    while (!g_tasks.empty()) {
        auto task = std::move(g_tasks.front());
        g_tasks.pop_front();
        task();
    }
    auto now = std::chrono::steady_clock::now();
    for (size_t i = 0; i < g_timers.size();) {
        if (g_timers[i].due <= now) {
            void *context = g_timers[i].context;
            g_timers.erase(g_timers.begin() + long(i));
            g_cdm->TimerExpired(context);
        } else {
            i++;
        }
    }
}

int wait_ms() {
    if (!g_tasks.empty()) return 0;
    int wait = 1000;
    auto now = std::chrono::steady_clock::now();
    for (auto &timer : g_timers) {
        auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(timer.due - now).count();
        wait = std::min(wait, int(std::max<long long>(0, ms)));
    }
    return wait;
}

/// `clear:cipher,...`, or `-` for a sample encrypted whole.
bool parse_map(const std::string &text, uint32_t size, std::vector<cdm::SubsampleEntry> &out) {
    out.clear();
    if (text == "-") return true;
    uint64_t total = 0;
    std::istringstream entries(text);
    std::string entry;
    while (std::getline(entries, entry, ',')) {
        unsigned long clear = 0, cipher = 0;
        if (std::sscanf(entry.c_str(), "%lu:%lu", &clear, &cipher) != 2) return false;
        total += uint64_t(clear) + cipher;
        out.push_back({uint32_t(clear), uint32_t(cipher)});
    }
    return total == size;
}

void decrypt(std::istringstream &words) {
    std::string kid_text, iv_text, map_text, data_text;
    std::vector<uint8_t> kid, iv, data;
    std::vector<cdm::SubsampleEntry> map;
    words >> kid_text >> iv_text >> map_text >> data_text;
    if (!b64decode(kid_text, kid) || !b64decode(iv_text, iv) || !b64decode(data_text, data) ||
        kid.size() != 16 || (iv.size() != 8 && iv.size() != 16) ||
        !parse_map(map_text, uint32_t(data.size()), map)) {
        emit("CLEAR " + std::to_string(cdm::kDecryptError) + " -");
        return;
    }
    // An 8-byte IV is the high half of the counter block.
    iv.resize(16, 0);
    cdm::InputBuffer_2 input{};
    input.data = data.data();
    input.data_size = uint32_t(data.size());
    input.encryption_scheme = cdm::EncryptionScheme::kCenc;
    input.key_id = kid.data();
    input.key_id_size = uint32_t(kid.size());
    input.iv = iv.data();
    input.iv_size = uint32_t(iv.size());
    input.subsamples = map.empty() ? nullptr : map.data();
    input.num_subsamples = uint32_t(map.size());
    Block block;
    cdm::Status status = g_cdm->Decrypt(input, &block);
    cdm::Buffer *out = block.DecryptedBuffer();
    if (status != cdm::kSuccess || !out || out->Size() != data.size()) {
        emit("CLEAR " + std::to_string(status == cdm::kSuccess ? cdm::kDecryptError : status) + " -");
        return;
    }
    emit("CLEAR 0 " + b64encode(out->Data(), out->Size()));
}

}  // namespace

int main(int argc, char **argv) {
    if (argc != 3) {
        std::fprintf(stderr, "usage: wvhost <libwidevinecdm.so> <state directory>\n");
        return 2;
    }
    g_state = argv[2];
    void *lib = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!lib) {
        emit("LOG the CDM did not load");
        emit("INITFAIL");
        return 1;
    }
    auto init_module = reinterpret_cast<void (*)()>(dlsym(lib, "InitializeCdmModule_4"));
    auto create = reinterpret_cast<void *(*)(int, const char *, uint32_t, GetCdmHostFunc, void *)>(
        dlsym(lib, "CreateCdmInstance"));
    if (!init_module || !create) {
        emit("LOG the CDM lacks its entry points");
        emit("INITFAIL");
        return 1;
    }
    init_module();
    static const char kKeySystem[] = "com.widevine.alpha";
    g_cdm = static_cast<cdm::ContentDecryptionModule_10 *>(create(
        cdm::ContentDecryptionModule_10::kVersion, kKeySystem, sizeof kKeySystem - 1, get_host, nullptr));
    if (!g_cdm) {
        emit("LOG the CDM refused interface 10");
        emit("INITFAIL");
        return 1;
    }
    g_cdm->Initialize(false, true, false);

    uint32_t promise = 1;
    std::string pending;
    for (;;) {
        run_due();
        size_t newline = pending.find('\n');
        if (newline == std::string::npos) {
            if (pending.size() > kMaxLine) break;
            pollfd fd{STDIN_FILENO, POLLIN, 0};
            if (poll(&fd, 1, wait_ms()) <= 0) continue;
            char chunk[65536];
            ssize_t got = read(STDIN_FILENO, chunk, sizeof chunk);
            if (got <= 0) break;
            pending.append(chunk, size_t(got));
            continue;
        }
        std::istringstream words(pending.substr(0, newline));
        pending.erase(0, newline + 1);
        std::string verb, arg;
        words >> verb;
        std::vector<uint8_t> bytes;
        if (verb == "QUIT") break;
        if (verb == "DECRYPT") {
            decrypt(words);
            continue;
        }
        words >> arg;
        if (!b64decode(arg, bytes)) {
            emit("LOG unreadable " + verb);
            continue;
        }
        if (verb == "CERT") {
            g_cdm->SetServerCertificate(promise++, bytes.data(), uint32_t(bytes.size()));
        } else if (verb == "PSSH") {
            g_cdm->CreateSessionAndGenerateRequest(promise++, cdm::kTemporary, cdm::kCenc, bytes.data(),
                                                   uint32_t(bytes.size()));
        } else if (verb == "UPDATE") {
            g_cdm->UpdateSession(promise++, g_session.data(), uint32_t(g_session.size()), bytes.data(),
                                 uint32_t(bytes.size()));
        } else {
            emit("LOG unknown " + verb);
        }
    }
    g_cdm->Destroy();
    return 0;
}
