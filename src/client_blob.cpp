#include "client_blob.hpp"
#include "containers.hpp"
#include "logging.hpp"
#include "memory.hpp"
#include "utils.hpp"

// FIXME:
// - Use smarter (binary) serialisation with versioning
// - Store user version in blob to prevent server old blob replay
// - Serialize memos so they compress better

namespace ga {
namespace sdk {

    namespace {
        static const std::string ZERO_HMAC_BASE64 = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

        constexpr uint32_t SA_NAMES = 0; // Subaccount names
        constexpr uint32_t TX_MEMOS = 1; // Transaction memos

        // blob prefix: 1 byte version, 3 reserved bytes
        static const std::array<unsigned char, 4> PREFIX{ 1, 0, 0, 0 };
    } // namespace

    client_blob::client_blob()
        : m_data()
    {
        m_data[SA_NAMES] = nlohmann::json();
        m_data[TX_MEMOS] = nlohmann::json();
    }

    void client_blob::set_subaccount_name(uint32_t subaccount, const std::string& name)
    {
        json_add_non_default(m_data[SA_NAMES], std::to_string(subaccount), name);
    }

    std::string client_blob::get_subaccount_name(uint32_t subaccount) const
    {
        return json_get_value(m_data[SA_NAMES], std::to_string(subaccount));
    }

    void client_blob::set_tx_memo(const std::string& txhash_hex, const std::string& memo)
    {
        json_add_non_default(m_data[TX_MEMOS], txhash_hex, memo);
    }

    std::string client_blob::get_tx_memo(const std::string& txhash_hex) const
    {
        return json_get_value(m_data[TX_MEMOS], txhash_hex);
    }

    bool client_blob::is_zero_hmac(const std::string& hmac) { return hmac == ZERO_HMAC_BASE64; }

    std::string client_blob::compute_hmac(byte_span_t hmac_key, byte_span_t data)
    {
        return base64_from_bytes(hmac_sha256(hmac_key, data));
    }

    void client_blob::load(byte_span_t key, byte_span_t data)
    {
        std::vector<unsigned char> decrypted(aes_gcm_decrypt_get_length(data));
        GDK_RUNTIME_ASSERT(decrypted.size() > PREFIX.size());
        GDK_RUNTIME_ASSERT(aes_gcm_decrypt(key, data, decrypted) == decrypted.size());

        // Only one fixed prefix value is currently allowed, check we match it
        GDK_RUNTIME_ASSERT(memcmp(decrypted.data(), PREFIX.data(), PREFIX.size()) == 0);

        const auto decompressed = decompress(gsl::make_span(decrypted).subspan(PREFIX.size()));
        m_data = nlohmann::json::parse(decompressed.begin(), decompressed.end());
    }

    std::pair<std::vector<unsigned char>, std::string> client_blob::save(byte_span_t key, byte_span_t hmac_key) const
    {
        const std::string data = m_data.dump();
        auto compressed = compress(PREFIX, ustring_span(data));
        std::vector<unsigned char> encrypted(aes_gcm_encrypt_get_length(compressed));
        GDK_RUNTIME_ASSERT(aes_gcm_encrypt(key, compressed, encrypted) == encrypted.size());
        auto hmac = compute_hmac(hmac_key, encrypted);
        return std::make_pair(std::move(encrypted), std::move(hmac));
    }

} // namespace sdk
} // namespace ga