extern crate openssl;
#[macro_use]
use std::str;
use serde::Serialize as Serialize2;
use serde_derive::{Deserialize, Serialize};
use actix_web::{
    get, post, web, HttpResponse, HttpRequest, Responder 
};
use log::{error, info};
extern crate sgx_types;
extern crate sgx_urts;
use sgx_types::*;
use sgx_urts::SgxEnclave;
use mysql::*;
use crate::ecall;
use crate::endpoint::utils::GenericError;
use crate::persistence::*;
use crate::endpoint::utils;
use crate::endpoint::auth_token;
use crate::endpoint::oauth::*;
use std::collections::HashMap;
use crate::endpoint::session;

static SUCC: &'static str = "success";
static FAIL: &'static str = "fail";

/// BaseResp is a base response for most request
/// status can either be:
/// SUCCESS or 
/// FAIL
#[derive(Debug, Serialize, Deserialize)]
pub struct BaseResp {
    status: String,
    error_code: String,
    error_msg: String
}


fn fail_resp(error_code: &str, error_msg: &str) -> HttpResponse {
    HttpResponse::Ok().json(BaseResp {
        status: FAIL.to_string(),
        error_code: error_code.to_string(),
        error_msg: error_msg.to_string()
    })
}

fn succ_resp() -> HttpResponse {
    HttpResponse::Ok().json(BaseResp {
        status: SUCC.to_string(),
        error_code: "".to_string(),
        error_msg: "".to_string()
    })
}

fn json_resp<S: Serialize2>(resp: S) -> HttpResponse {
    HttpResponse::Ok().json(resp)
}

/// AppState includes:
/// enclave instance, db_pool instance and config instance
/// It is Passed to every request handler
#[derive(Debug)]
pub struct AppState {
    pub enclave: SgxEnclave,
    pub thread_pool: rayon::ThreadPool,
    pub db_pool: Pool,
    pub conf: HashMap<String, String>
}


/// Exchange Key Request includes a user public key for secure channel
#[derive(Deserialize)]
pub struct ExchangeKeyReq {
    key: String
}

/// Exchange key Response returns tee public key
#[derive(Debug, Serialize, Deserialize)]
pub struct ExchangeKeyResp {
    status: String,
    key: String,
    session_id: String
}

fn sgx_success(t: sgx_status_t) -> bool {
    t == sgx_status_t::SGX_SUCCESS
}

/* Exchange key function takes exchange key req from user as user pub key,
   and return exchange key resp as tee pub key.
   Browser accept pub key format as 04 + hex of pub key,
   while tee accept pub key format as [u8;64].
   Remove 04 before send to tee and add 04 before send to browser.
*/
#[post("/dauth/exchange_key")]
pub async fn exchange_key(
    req: web::Json<ExchangeKeyReq>,
    endex: web::Data<AppState>,
    sessions: web::Data<session::SessionState>    
) ->  impl Responder {
    info!("exchange key with {}", &req.key);
    let e = &endex.enclave;
    let pool = &endex.thread_pool;
    let mut sgx_result = sgx_status_t::SGX_SUCCESS;
    // remove 04 from pub key
    let user_key: [u8;64] = hex::decode(&req.key[2..]).unwrap().try_into().unwrap();
    let mut out_key: [u8;64] = [0; 64];
    let mut session_id: [u8;32] = [0;32];
    let result = pool.install(|| {
        unsafe {
            ecall::ec_key_exchange(
                e.geteid(), 
                &mut sgx_result, 
                &user_key,
                &mut out_key,
                &mut session_id
            )
        }
    });
    if !sgx_success(result) {
        error!("unsafe call failed.");
        return fail_resp("SgxError", "unsafe call failed");
    }
    if !sgx_success(sgx_result) {
        error!("sgx return error.");
        return fail_resp("SgxError", "sgx return error");
    }
    let out_key_hex = hex::encode(&out_key);
    let session_id_hex = hex::encode(&session_id);
    info!("exchange key from sgx {} {}", &out_key_hex, &session_id_hex);
    sessions.register_session(&session_id_hex);
    json_resp(ExchangeKeyResp {
        status: SUCC.to_string(),
        key: format!("04{}", out_key_hex),  // add 04 before send to browser
        session_id: session_id_hex
    })
}


#[derive(Deserialize)]
pub struct AuthEmailReq {
    session_id: String,
    cipher_email: String,
}

// with BaseResp
#[post("/dauth/auth_email")]
pub async fn auth_email(
    req: web::Json<AuthEmailReq>,
    endex: web::Data<AppState>,
    sessions: web::Data<session::SessionState>
) -> HttpResponse {
    info!("auth email with session_id {}", &req.session_id);
    // validate session
    // TODO: add a function to sessions with name validate_session
    if let None = sessions.get_session(&req.session_id) {
        info!("session not found");
        return fail_resp("DataError", "session not found");
    }
    let session = sessions.get_session(&req.session_id).unwrap();
    let session_id_b: [u8;32] = hex::decode(&req.session_id).unwrap().try_into().unwrap();
    let e = &endex.enclave;
    if session.expire() {
        info!("session expired");
        sessions.close_session(&req.session_id);
        close_ec_session(e.geteid(), &session_id_b);
        return fail_resp("DataError", "session expired");
    }

    let email_b = hex::decode(&req.cipher_email).unwrap();
    let mut sgx_result = sgx_status_t::SGX_SUCCESS;
    // sendmail

    let pool = &endex.thread_pool;
    let result = pool.install(|| {
        unsafe {
            ecall::ec_send_cipher_email(
                e.geteid(), 
                &mut sgx_result, 
                &session_id_b,
                email_b.as_ptr() as *const u8,
                email_b.len(),
            )
        }
    }); 
    if !sgx_success(result) {
        error!("unsafe call failed.");
        return fail_resp("SgxError", "unsafe call failed");
    }
    if !sgx_success(sgx_result) {
        error!("sgx return error.");
        return fail_resp("SgxError", "sgx return error");
    }
    // TODO: email not able to send, handle error
    succ_resp()
}

#[derive(Deserialize)]
pub struct AuthEmailConfirmReq {
    session_id: String,
    cipher_code: String
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthSuccessResp {
    status: String,
    token: String
}

#[post("/dauth/auth_email_confirm")]
pub async fn auth_email_confirm(
    req: web::Json<AuthEmailConfirmReq>,
    endex: web::Data<AppState>,
    sessions: web::Data<session::SessionState>
) -> HttpResponse {
    info!("register mail confirm with session_id {}", &req.session_id);
    // verify session_id
    if let None = sessions.get_session(&req.session_id) {
        info!("session not found");
        return fail_resp("DataError", "session not found");
    }

    let session = sessions.get_session(&req.session_id).unwrap();
    let session_id_b: [u8;32] = hex::decode(&req.session_id).unwrap().try_into().unwrap();
    let e = &endex.enclave;
    let pool = &endex.thread_pool;
    let mut sgx_result = sgx_status_t::SGX_SUCCESS;
    if session.expire() {
        info!("session expired");
        sessions.close_session(&req.session_id);
        close_ec_session(e.geteid(), &session_id_b);
        return fail_resp("DataError", "session expired");
    }

    let code_b = hex::decode(&req.cipher_code).unwrap();
    let mut email_hash = [0_u8;32];
    let mut email_seal = [0_u8;1024];
    let mut email_size = 0;
    let result = pool.install(|| {
        unsafe {
            ecall::ec_register_email_confirm(
                e.geteid(), 
                &mut sgx_result, 
                &session_id_b,
                code_b.as_ptr() as *const u8,
                code_b.len(),
                &mut email_hash,
                &mut email_seal,
                &mut email_size,
            )
        }
    });
    if !sgx_success(result) {
        error!("unsafe call failed.");
        return fail_resp("SgxError", "unsafe call failed");
    }
    if !sgx_success(sgx_result) {
        error!("sgx return error.");
        return fail_resp("SgxError", "sgx return error");
    }
    if sgx_result == sgx_status_t::SGX_ERROR_INVALID_ATTRIBUTE {
        info!("OAuth failed");
        return fail_resp("OsError", "OAuth failed");
    }
    let email_hash_hex = hex::encode(email_hash);
    let size: usize = email_size.try_into().unwrap();
    let email_seal_hex = hex::encode(&email_seal[..size]);
    // after confirm mail success, if new email, insert_mail
    // increase auth_hist
    // return token
    let account = Account {
        acc_hash: email_hash_hex.clone(),
        acc_seal: email_seal_hex,
    };
    insert_account_if_new(&endex.db_pool, &account);
    let a_id = query_latest_auth_id(&endex.db_pool, &account.acc_hash);
    let next_id = a_id + 1;
    let auth =  Auth {
        acc_hash: email_hash_hex.clone(),
        auth_id: next_id,
        auth_type: AuthType::Email,
        auth_datetime: utils::now_datetime().unwrap(),
        auth_exp: utils::system_time() + 3600,
    };
    let token_r = sign_auth_jwt(e.geteid(), pool, &auth);
    if token_r.is_err() {
        return fail_resp("SGXError", "sign auth failed");
    }
    let token = token_r.unwrap();
    insert_auth(&endex.db_pool, auth);
    json_resp(
        AuthSuccessResp{
            status: SUCC.to_string(),
            token: token
        }
    )
}


#[derive(Debug, Serialize, Deserialize)]
pub struct AuthOauthReq {
    session_id: String,
    cipher_code: String,
    oauth_type: String
}


#[post("/dauth/auth_oauth2")]
pub async fn auth_oauth2(
    req: web::Json<AuthOauthReq>,
    http_req: HttpRequest,
    endex: web::Data<AppState>,
    sessions: web::Data<session::SessionState>
) -> HttpResponse {
    info!("github oauth with session_id {}", &req.session_id);
    // verify session_id
    if let None = sessions.get_session(&req.session_id) {
        info!("session not found");
        return fail_resp("DataError", "session not found");
    }

    let session = sessions.get_session(&req.session_id).unwrap();
    let session_id_b: [u8;32] = hex::decode(&req.session_id).unwrap().try_into().unwrap();
    let e = &endex.enclave;
    let pool = &endex.thread_pool;
    let mut sgx_result = sgx_status_t::SGX_SUCCESS;
    if session.expire() {
        info!("session expired");
        sessions.close_session(&req.session_id);
        close_ec_session(e.geteid(), &session_id_b);
        return fail_resp("DataError", "session expired");
    }
    let auth_type_r = AuthType::from_str(&req.oauth_type);
    if auth_type_r.is_none() {
        return fail_resp("ReqError", "oauth type not found");
    }
    let auth_type = auth_type_r.unwrap();
    let code_b = hex::decode(&req.cipher_code).unwrap();
    let mut acc_hash = [0_u8;32];
    let mut acc_seal = [0_u8;1024];
    let mut acc_seal_size = 0;

    let result = unsafe {
        ecall::ec_auth_oauth(
            e.geteid(), 
            &mut sgx_result, 
            &session_id_b,
            code_b.as_ptr() as *const u8,
            code_b.len(),
            auth_type as i32,
            &mut acc_hash,
            &mut acc_seal,
            &mut acc_seal_size
        )
    };
    info!("unsafe result is {:?}", &result);
    if result != sgx_status_t::SGX_SUCCESS {
        return fail_resp("SGXError", "");
    }
    info!("sgx result is {:?}", &sgx_result);
    if sgx_result != sgx_status_t::SGX_SUCCESS {
        match sgx_result {
            sgx_status_t::SGX_ERROR_INVALID_ATTRIBUTE => {
                info!("confirm code does not match");
                return fail_resp("DataError", "confirm code does not match");
            },
            _ => {
                error!("sgx failed.");
                return fail_resp("SgxError", "");
            },
        }
    }
    let auth_hash_hex = hex::encode(acc_hash);
    let size: usize = acc_seal_size.try_into().unwrap();
    let auth_seal_hex = hex::encode(&acc_seal[..size]);
    // after oauth success, if new oauth, update, else do nothing
    // insert auth
    let auth_account = Account {
        acc_hash: auth_hash_hex.clone(),
        acc_seal: auth_seal_hex,
    };
    insert_account_if_new(&endex.db_pool, &auth_account);
    let a_id = query_latest_auth_id(&endex.db_pool, &auth_hash_hex);
    let next_id = a_id + 1;
    let auth = Auth {
        acc_hash: auth_hash_hex.clone(),
        auth_id: next_id,
        auth_type: auth_type,        
        auth_datetime: utils::now_datetime().unwrap(),
        auth_exp: utils::system_time() + 3600,
    };
    let token_r = sign_auth_jwt(e.geteid(), pool, &auth);
    if token_r.is_err() {
        return fail_resp("SGXError", "sign auth failed");
    }
    let token = token_r.unwrap();
    insert_auth(&endex.db_pool, auth);
    json_resp(AuthSuccessResp{
        status: SUCC.to_string(),
        token: token
    })
}


#[post("/dauth/auth_oauth")]
pub async fn auth_oauth(
    req: web::Json<AuthOauthReq>,
    http_req: HttpRequest,
    endex: web::Data<AppState>,
    sessions: web::Data<session::SessionState>
) -> HttpResponse {
    info!("github oauth with session_id {}", &req.session_id);
    // verify session_id
    if let None = sessions.get_session(&req.session_id) {
        info!("session not found");
        return fail_resp("DataError", "session not found");
    }

    let session = sessions.get_session(&req.session_id).unwrap();
    let session_id_b: [u8;32] = hex::decode(&req.session_id).unwrap().try_into().unwrap();
    let e = &endex.enclave;
    let pool = &endex.thread_pool;
    let mut sgx_result = sgx_status_t::SGX_SUCCESS;
    if session.expire() {
        info!("session expired");
        sessions.close_session(&req.session_id);
        close_ec_session(e.geteid(), &session_id_b);
        return fail_resp("DataError", "session expired");
    }
    
    // get audience or client id
    let claims = auth_token::extract_token(
        http_req.headers().get("Authorization"),
        &endex.conf["secret"].as_str()
    );
    if claims.is_none() {
        return fail_resp("DataError", "invalid token");
    }
    let claims2 = claims.unwrap();

    let auth_type_r = AuthType::from_str(&req.oauth_type);
    if auth_type_r.is_none() {
        return fail_resp("ReqError", "oauth type not found");
    }
    let auth_type = auth_type_r.unwrap();
    let result = match auth_type {
        AuthType::Google => google_oauth(
            &endex.conf["google_client_id"], 
            &endex.conf["google_client_secret"],
            &endex.conf["google_redirect_url"],
            &req.cipher_code),
        AuthType::Github => github_oauth(
            &endex.conf["github_client_id"], 
            &endex.conf["github_client_secret"],
            &req.cipher_code),
        _ => Err(GenericError::from("wrong auth type"))
    };
    if result.is_err() {
        error!("oauth call failed.");
        return fail_resp("OauthError", &result.unwrap().to_string());
    }
    let oauth_value = result.unwrap();
    let oauth_with_type = format!(
        "{}@{}",
        oauth_value,
        auth_type.to_string()
    );
    let oauth_value_b = oauth_with_type.as_bytes();
    info!("oauth value {:?}", &oauth_with_type);
    let mut auth_hash = [0_u8;32];
    let mut auth_seal = [0_u8;1024];
    let mut auth_size = 0;

    let result2 = pool.install(|| {
        unsafe {
            ecall::ec_seal(
                endex.enclave.geteid(),
                &mut sgx_result,
                oauth_value_b.as_ptr() as *const u8,
                oauth_value_b.len(),
                &mut auth_hash,
                &mut auth_seal,
                &mut auth_size
            )
        }
    }); 
    if !sgx_success(result2) {
        error!("unsafe error.");
        return fail_resp("SgxError", "");
    }
    if !sgx_success(sgx_result) {
        error!("sgx return error.");
        return fail_resp("SgxError", "sgx return error");
    }
    let auth_hash_hex = hex::encode(auth_hash);
    let size: usize = auth_size.try_into().unwrap();
    let auth_seal_hex = hex::encode(&auth_seal[..size]);
    let account = Account {
        acc_hash: auth_hash_hex.clone(),
        acc_seal: auth_seal_hex,
    };
    insert_account_if_new(&endex.db_pool, &account);
    let a_id = query_latest_auth_id(&endex.db_pool, &auth_hash_hex);
    let next_id = a_id + 1;
    let exp = utils::system_time() + 3600;
    let auth = Auth {
        acc_hash: auth_hash_hex.clone(),
        auth_id: next_id,
        auth_type: auth_type,        
        auth_datetime: utils::now_datetime().unwrap(),
        auth_exp: utils::system_time() + 3600,
    };
    let token_r = sign_auth_jwt(e.geteid(), pool, &auth);
    if token_r.is_err() {
        return fail_resp("SGXError", "sign auth failed");
    }
    let token = token_r.unwrap();
    insert_auth(&endex.db_pool, auth);
    json_resp(AuthSuccessResp{
        status: SUCC.to_string(),
        token: token
    })
}



fn sign_auth(
    eid: sgx_enclave_id_t, 
    pool: &rayon::ThreadPool, 
    a_hash: &[u8;32], 
    a_id: i32,
    exp: u64
) -> utils::GenericResult<(String,String)> {
    info!("sign auth with id {} hash {:?}", a_id, a_hash);
    let mut sgx_result = sgx_status_t::SGX_SUCCESS;
    let mut pub_k: [u8;65] = [0_u8;65];
    let mut signature: [u8;65] = [0_u8;65];
    let result = pool.install(|| {
        unsafe {
            ecall::ec_sign_auth(
                eid,
                &mut sgx_result,
                &a_hash,
                a_id,
                exp,
                &mut pub_k,
                &mut signature
            )
        }
    }); 
    match result {
        sgx_status_t::SGX_SUCCESS => {
            let signature_hex = hex::encode(signature);
            let pub_k_hex = hex::encode(pub_k);
            Ok((pub_k_hex, signature_hex))
        },
        _ => {
            error!("sgx failed.");
            Err(GenericError::from("ec_auth_sign failed"))
        }
    }

}


fn sign_auth_jwt(
    eid: sgx_enclave_id_t, 
    pool: &rayon::ThreadPool, 
    auth: &Auth
) -> utils::GenericResult<String> {
    info!("sign auth for {:?} {} times", &auth.acc_hash, &auth.auth_id);
    let mut sgx_result = sgx_status_t::SGX_SUCCESS;
    let hash_b = hex::decode(&auth.acc_hash).unwrap().try_into().unwrap();
    let mut token: [u8;1024] = [0_u8;1024];
    let mut token_size = 0;
    let result = pool.install(|| {
        unsafe {
            ecall::ec_sign_auth_jwt(
                eid,
                &mut sgx_result,
                &hash_b,
                auth.auth_id,
                auth.auth_exp,
                &mut token,
                &mut token_size
            )
        }
    }); 
    match result {
        sgx_status_t::SGX_SUCCESS => {
            let size: usize = token_size.try_into().unwrap();
            let token_s = str::from_utf8(&token[..size]).unwrap();
            Ok(token_s.to_string())
        },
        _ => {
            error!("sgx failed.");
            Err(GenericError::from("ec_auth_sign failed"))
        }
    }

}


fn close_ec_session(eid: sgx_enclave_id_t, session_id_b: &[u8;32]) {
    let mut sgx_result = sgx_status_t::SGX_SUCCESS;
    unsafe {
        ecall::ec_close_session(
            eid,
            &mut sgx_result,
            &session_id_b
        );
    }
}


#[get("/dauth/health")]
pub async fn health(endex: web::Data<AppState>) -> impl Responder {
    // for health check
    HttpResponse::Ok().body("Webapp is up and running!")
}


