//! HTTP セッション認証（NoSQL 表層。TASK-174・HTTP-4・HTTP-5・HTTP-8）の構成要素群。
//!
//! `POST /v1/session` での認証成功時に発行し、以後のリクエストで
//! `Authorization: Bearer <token>` として提示される不透明セッショントークンを
//! 中心に、認証フロー全体を小さな構成要素へ分割する。本モジュールが担うのは
//! [`token`]（トークンの生成・base64url 表現・パース）・[`store`]
//! （発行済みトークン → `PolicyContext` の対応・TTL・同時有効数上限を持つ
//! メモリ内ストア。Issue #751）・[`issue`]（`POST /v1/session` の発行
//! パイプライン本体。本文検証→`crate::auth::verify`→`store::SessionStore::issue`。
//! Issue #752・HTTP-6）・[`bearer`]（`Authorization: Bearer` ヘッダの受信データ
//! 経路。[`store::SessionStore`] のトークン検証に必要な `SessionToken` への
//! 変換。Issue #753・HTTP-8）・[`close`]（`POST /v1/session/close` のワンタイム
//! 失効パイプライン本体。`bearer::extract_bearer_token`→本文検証→
//! `store::SessionStore::close`。Issue #753・HTTP-8）・[`middleware`]
//! （`POST /v1/query` 前段の `Authorization: Bearer` ミドルウェア。[`bearer`]・
//! [`store::SessionStore::lookup`] を再利用して [`middleware::
//! SessionPrincipal`]（唯一のテナント文脈）を導出し、`tenant_id` 相当ヘッダの
//! 拒否も担う。Issue #754・HTTP-5・HTTP-6・HTTP-7）・[`scope`]（発行時の任意 `scope`
//! 〔op・表・読み取り専用〕の型と検証。トークンへ束縛して運ぶだけで強制は後続。
//! Issue #2100・HTTP-15）を提供する。

pub mod bearer;
pub mod close;
pub mod issue;
pub mod middleware;
pub mod scope;
pub mod store;
pub mod token;
