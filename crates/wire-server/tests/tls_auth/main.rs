//! 結合テストの束ね先 `tls_auth`。各ファイルを `mod` として束ね、テストバイナリ数を減らす
//! （方針・個別実行方法は README「wire-server の結合テストの束ね方と個別実行」）。

#[path = "../common/mod.rs"]
mod common;
#[path = "../common/tls_client.rs"]
mod tls_client;

mod tls_aes128_vectors;
mod tls_aes_gcm_vectors;
mod tls_channel_binding;
mod tls_ed25519_rfc8032;
mod tls_key_schedule_rfc8448;
mod tls_pem_pkcs8;
mod tls_record_protection_rfc8448;
mod tls_scram_design_doc;
mod tls_sha512_vectors;
mod tls_transcript_finished_rfc8448;
mod tls_x509;
mod wire_scram_mock_key_file_size;
