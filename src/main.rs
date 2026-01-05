// Rust Sniper - Dev Wallet Copy Trading Sniper
// Мониторит создание токенов через GRPC и покупает если creator в списке отслеживаемых девов

mod grpc_client;

use axum::{
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use std::collections::HashSet;
use std::str::FromStr;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
    transaction::TransactionError,
    instruction::InstructionError,
};
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_transaction_status::UiTransactionEncoding;
use anyhow::{Result, Context};
use tracing::{info, warn, error, debug};
use bs58;
use std::convert::TryInto;
use tokio::io::{AsyncReadExt};
use grpc_client::CreateTransaction;
use bincode;
use reqwest::Client; // Нужен для HTTP запросов к API тасков
use spl_associated_token_account::get_associated_token_address_with_program_id;
use futures::StreamExt;

// Конфигурация снайпера (загружается из тасков)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SniperConfig {
    pub auto_buy: bool,
    pub amount: f64,          // SOL
    pub slippage: f64,        // %
    pub min_liquidity: f64,   // SOL
    pub priority_fee: f64,    // SOL (total priority fee из config)
    pub compute_units: u32,   // Compute units limit
    pub wallet_private_key: String, // Base58 private key
    pub use_validators_only: Option<bool>, // Отключить RPC, использовать валидаторы напрямую
    pub validator_type: Option<String>, // Тип валидатора: "rpc", "jito_bundle", "direct"
    pub purchase_validator: Option<String>, // Валидатор для покупки (опционально)
    pub profit_threshold: f64, // % profit for sell
    pub loss_threshold: f64, // % loss for stop
    pub auto_sell_enabled: bool, // Enable auto-sell
    pub grpc_endpoint: Option<String>, // GRPC endpoint для мониторинга позиций
    pub api_token: Option<String>, // API token для GRPC
    pub tip_fee: f64, // SOL for validator tip (0.001-0.01 for speed) - только для Jito bundles
    pub tip_account: String, // Pubkey for validator tip
    pub sim_only: bool, // Если true, только симуляция, не отправка
    pub low_latency: bool, // Если true, skip simulation и bonding read для максимальной скорости
    pub assume_initial_curve: bool, // Если true, используем assumed initial curve state вместо RPC read
    pub min_tokens_out_override: Option<u64>, // Override для min_tokens_out (0 = no check)
}

// Структура задачи (task)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub running: bool,
    pub config: TaskConfig,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    pub amount: f64,
    pub slippage: f64,
    pub priority_fee: Option<f64>,
    pub compute_units: Option<u32>,
    pub wallet_id: Option<String>,
    pub wallet_address: Option<String>,
    pub wallet_private_key: String,
    pub devs: Vec<String>, // Список отслеживаемых dev адресов (config.devs)
    pub use_validators_only: Option<bool>, // Отключить RPC, использовать валидаторы напрямую
    pub validator_type: Option<String>, // Тип валидатора: "rpc", "jito_bundle", "direct"
    pub purchase_validator: Option<String>, // Валидатор для покупки (опционально)
    pub profit_threshold: Option<f64>,
    pub loss_threshold: Option<f64>,
    pub auto_sell_enabled: Option<bool>,
    pub grpc_endpoint: Option<String>, // GRPC endpoint для мониторинга позиций
    pub api_token: Option<String>, // API token для GRPC
    pub tip_fee: Option<f64>, // Validator tip fee (только для Jito bundles)
    pub tip_account: Option<String>, // Validator tip account
    pub sim_only: Option<bool>, // Если true, только симуляция
    pub low_latency: Option<bool>, // Если true, skip simulation и bonding read для максимальной скорости
    pub assume_initial_curve: Option<bool>, // Если true, используем assumed initial curve state вместо RPC read
    pub min_tokens_out_override: Option<u64>, // Override для min_tokens_out (0 = no check)
}

#[derive(Debug, Clone)]
pub struct Position {
    mint: String,
    bonding_curve: Pubkey,
    held_tokens: u64,
    buy_sol: f64,
}

// Состояние снайпера
#[derive(Debug)]
pub struct SniperState {
    pub running: bool,
    pub config: Option<SniperConfig>,
    pub tracked_devs: HashSet<String>,  // Список отслеживаемых creator_address
    pub stats: SniperStats,
    pub wallet_keypair: Option<Keypair>, // Keypair не реализует Clone
    pub token_purchased: bool, // Флаг: был ли уже куплен токен в этой сессии
    pub positions: Vec<Position>,
    pub grpc_client: Option<Arc<grpc_client::GrpcClient>>, // GRPC клиент для мониторинга позиций
    pub cached_fee_recipient: Option<Pubkey>, // Кеш для fee_recipient (статический)
    pub cached_event_authority: Option<Pubkey>, // Кеш для event_authority
    pub cached_fee_config: Option<Pubkey>, // Кеш для fee_config
    pub cached_blockhash: Option<solana_sdk::hash::Hash>, // Кеш для blockhash (обновляется каждые 2s)
    pub cached_blockhash_slot: Option<u64>, // Slot для cached blockhash
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SniperStats {
    pub transactions_sent: u64,
    pub successful: u64,
    pub failed: u64,
    pub tokens_created: u64,
    pub tokens_sniped: u64,
    pub errors: u64,
}

type AppState = Arc<RwLock<SniperState>>;

// Константы
const PUMP_FUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const MIGRATION_TRACKER_API: &str = "http://localhost:8241";
const SETTINGS_API: &str = "http://localhost:8242";
const RPC_URL: &str = "http://fr.rpc.gadflynode.com:80";
// Используем GRPC прокси вместо прямого подключения
// Прокси работает на Node.js и использует @triton-one/yellowstone-grpc
const GRPC_PROXY_TCP_HOST: &str = "127.0.0.1";
const GRPC_PROXY_TCP_PORT: u16 = 8725; // TCP socket для максимальной скорости
// Token-2022 Program ID (используется вместо стандартного Token Program)
const TOKEN_2022_PROGRAM_ID: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";

// Структура общих настроек
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeneralSettings {
    grpc_rpc_endpoint: Option<String>,
}

// API Handlers

async fn update_config(
    State(state): State<AppState>,
    Json(config): Json<SniperConfig>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut sniper = state.write().await;
    
    // Парсим приватный ключ
    match parse_private_key(&config.wallet_private_key) {
        Ok(keypair) => {
            sniper.wallet_keypair = Some(keypair);
            info!("✅ Wallet keypair loaded successfully");
        }
        Err(e) => {
            error!("❌ Failed to parse private key: {}", e);
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    
    sniper.config = Some(config);
    
    Ok(Json(serde_json::json!({
        "status": "updated",
        "message": "Configuration updated"
    })))
}

async fn start_sniper(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut sniper = state.write().await;
    
    if sniper.running {
        return Err(StatusCode::BAD_REQUEST);
    }
    
    // Загружаем настройки из тасков (обязательно перед запуском)
    // Снайпер использует ТОЛЬКО настройки из тасков
    info!("📋 Loading settings from tasks...");
    if let Err(e) = load_tracked_devs(&mut sniper).await {
        error!("❌ Failed to load task settings: {}", e);
        return Err(StatusCode::BAD_REQUEST);
    }
    
    // Проверяем что настройки загружены
    if sniper.config.is_none() {
        error!("❌ No active tasks found. Create and enable a task first.");
        return Err(StatusCode::BAD_REQUEST);
    }
    
    if sniper.wallet_keypair.is_none() {
        error!("❌ Wallet keypair not configured in task.");
        return Err(StatusCode::BAD_REQUEST);
    }
    
    if sniper.tracked_devs.is_empty() {
        warn!("⚠️ No addresses to track. Add addresses to task.");
    } else {
        info!("✅ Tracking {} addresses", sniper.tracked_devs.len());
    }
    
    info!("✅ Sniper configured from tasks. Starting...");
    info!("📊 Configuration:");
    if let Some(ref config) = sniper.config {
        info!("   - Amount: {} SOL", config.amount);
        info!("   - Slippage: {}%", config.slippage);
        info!("   - Priority fee: {} SOL", config.priority_fee);
        info!("   - Compute units: {}", config.compute_units);
        info!("   - Tip fee: {} SOL to {}", config.tip_fee, config.tip_account);
    }
    sniper.running = true;
    
    // Подключаемся к GRPC для мониторинга позиций, если включена автопродажа
    if let Some(ref config) = sniper.config {
        if config.auto_sell_enabled {
            if let (Some(endpoint), Some(token)) = (config.grpc_endpoint.as_ref(), config.api_token.as_ref()) {
                info!("🔌 Connecting to GRPC for position monitoring...");
                let mut grpc = grpc_client::GrpcClient::new(endpoint.clone(), token.clone());
                if let Err(e) = grpc.connect().await {
                    error!("❌ Failed to connect to GRPC: {}", e);
                } else {
                    sniper.grpc_client = Some(Arc::new(grpc));
                    info!("✅ GRPC connected for position monitoring");
                }
            } else {
                warn!("⚠️ Auto-sell enabled but GRPC endpoint/token not configured");
            }
        }
    }
    
    // Запускаем снайпер в отдельной задаче
    let sniper_state = state.clone();
    tokio::spawn(async move {
        run_sniper_loop(sniper_state).await;
    });
    
    Ok(Json(serde_json::json!({
        "status": "started",
        "message": "Sniper started successfully",
        "tracked_devs_count": sniper.tracked_devs.len()
    })))
}

async fn stop_sniper(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut sniper = state.write().await;
    sniper.running = false;
    
    Ok(Json(serde_json::json!({
        "status": "stopped",
        "message": "Sniper stopped"
    })))
}

async fn get_status(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let sniper = state.read().await;
    
    Json(serde_json::json!({
        "running": sniper.running,
        "config": sniper.config,
        "tracked_devs_count": sniper.tracked_devs.len(),
        "stats": sniper.stats
    }))
}

async fn get_stats(
    State(state): State<AppState>,
) -> Json<SniperStats> {
    let sniper = state.read().await;
    Json(sniper.stats.clone())
}

// Парсинг приватного ключа из base58
fn parse_private_key(key_str: &str) -> Result<Keypair> {
    let key_bytes = bs58::decode(key_str.trim())
        .into_vec()
        .context("Failed to decode base58 private key")?;
    
    if key_bytes.len() == 64 {
        Keypair::try_from(&key_bytes[..])
            .context("Failed to create keypair from bytes")
    } else {
        anyhow::bail!("Invalid key length: expected 64 bytes, got {}", key_bytes.len())
    }
}

// Загрузка списка отслеживаемых девов из tasks и настройка кошельков
async fn load_tracked_devs(sniper: &mut SniperState) -> Result<()> {
    info!("🔄 Loading tracked devs from tasks...");
    info!("📡 Fetching from: {}/api/tasks", MIGRATION_TRACKER_API);
    
    let client = Client::new();
    let url = format!("{}/api/tasks", MIGRATION_TRACKER_API);
    
    let response = client.get(&url).send().await.context("Failed to fetch tasks")?;
    info!("📥 Response status: {}", response.status());
    
    if !response.status().is_success() {
        error!("❌ Failed to fetch tasks: {}", response.status());
        return Err(anyhow::anyhow!("Failed to fetch tasks: {}", response.status()));
    }
    
    let data: serde_json::Value = response.json().await.context("Failed to parse tasks")?;
    info!("📦 Received tasks data");
    
    sniper.tracked_devs.clear();
    
    // Загружаем первую активную задачу для получения настроек
    let mut active_task: Option<Task> = None;
    
    if let Some(tasks) = data.get("tasks").and_then(|v| v.as_array()) {
        for task_json in tasks {
            // Проверяем, что задача включена и запущена
            let enabled = task_json.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
            let running = task_json.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
            
            if enabled && running {
                // Парсим задачу
                if let Ok(task) = serde_json::from_value::<Task>(task_json.clone()) {
                    // Добавляем dev адреса из config.devs
                    if let Some(devs) = task_json.get("config").and_then(|c| c.get("devs")).and_then(|v| v.as_array()) {
                        for dev in devs {
                            if let Some(addr) = dev.as_str() {
                                sniper.tracked_devs.insert(addr.to_string());
                            }
                        }
                    }
                    
                    // Сохраняем первую активную задачу для настроек
                    if active_task.is_none() {
                        active_task = Some(task);
                    }
                }
            }
        }
    }
    
    // Если есть активная задача, обновляем конфигурацию снайпера
    if let Some(task) = active_task {
        info!("📋 Using settings from task: {}", task.name);
        info!("   - Task ID: {}", task.id);
        info!("   - Enabled: {}", task.enabled);
        info!("   - Running: {}", task.running);
        
        // Создаем конфигурацию из задачи
        let config = SniperConfig {
            auto_buy: true,
            amount: task.config.amount,
            slippage: task.config.slippage,
            min_liquidity: 0.0, // Можно добавить в задачу позже
            priority_fee: task.config.priority_fee.unwrap_or(0.001),
            compute_units: task.config.compute_units.unwrap_or(200000),
            wallet_private_key: task.config.wallet_private_key.clone(),
            use_validators_only: task.config.use_validators_only,
            validator_type: task.config.validator_type.clone(),
            purchase_validator: task.config.purchase_validator.clone(),
            profit_threshold: task.config.profit_threshold.unwrap_or(200.0),
            loss_threshold: task.config.loss_threshold.unwrap_or(-10.0),
            auto_sell_enabled: task.config.auto_sell_enabled.unwrap_or(false),
            grpc_endpoint: task.config.grpc_endpoint.clone(),
            api_token: task.config.api_token.clone(),
            tip_fee: task.config.tip_fee.unwrap_or(0.0),
            tip_account: task.config.tip_account.unwrap_or_else(|| "295Avbam4qGShBYK7E9H5Ldew4B3WyJGmgmXfiWdeeyV".to_string()),
            sim_only: task.config.sim_only.unwrap_or(false),
            low_latency: true, // По умолчанию включен для максимальной скорости
            assume_initial_curve: true, // По умолчанию используем assumed state
            min_tokens_out_override: None,
        };
        
        // Кешируем статические данные для скорости
        if config.low_latency {
            let rpc_client = RpcClient::new(RPC_URL.to_string());
            let pump_fun_program = Pubkey::from_str(PUMP_FUN_PROGRAM_ID).unwrap();
            let (global_pda, _) = Pubkey::find_program_address(&[b"global"], &pump_fun_program);
            
            // Загружаем fee_recipient
            if let Ok(global_data) = rpc_client.get_account_data(&global_pda).await {
                if global_data.len() >= 73 {
                    if let Ok(fee_recipient) = Pubkey::try_from(&global_data[41..73]) {
                        sniper.cached_fee_recipient = Some(fee_recipient);
                    }
                }
            }
            
            let (event_authority, _) = Pubkey::find_program_address(&[b"__event_authority"], &pump_fun_program);
            sniper.cached_event_authority = Some(event_authority);
            
            let fee_program = Pubkey::from_str("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ").unwrap();
            let fee_config_seed = [
                1, 86, 224, 246, 147, 102, 90, 207, 68, 219, 21, 104, 191, 23, 91, 170,
                81, 137, 203, 151, 245, 210, 255, 59, 101, 93, 43, 182, 253, 109, 24, 176,
            ];
            let (fee_config, _) = Pubkey::find_program_address(&[b"fee_config", &fee_config_seed], &fee_program);
            sniper.cached_fee_config = Some(fee_config);
        }
        
        // Парсим приватный ключ
        match parse_private_key(&task.config.wallet_private_key) {
            Ok(keypair) => {
                sniper.wallet_keypair = Some(keypair);
                info!("✅ Wallet keypair loaded from task");
            }
            Err(e) => {
                error!("❌ Failed to parse private key from task: {}", e);
            }
        }
        
        sniper.config = Some(config);
    }
    
    info!("✅ Loaded {} tracked dev wallets from tasks", sniper.tracked_devs.len());
    if sniper.tracked_devs.len() > 0 {
        info!("📋 Tracked addresses:");
        for (i, dev) in sniper.tracked_devs.iter().take(10).enumerate() {
            info!("   {}. {}", i + 1, dev);
        }
        if sniper.tracked_devs.len() > 10 {
            info!("   ... and {} more", sniper.tracked_devs.len() - 10);
        }
    }
    
    Ok(())
}

// Проверка, есть ли dev в базе данных
#[allow(dead_code)]
async fn check_dev_in_database(creator_address: &str) -> Result<bool> {
    let client = Client::new();
    let url = format!("{}/api/dev-wallets", MIGRATION_TRACKER_API);
    
    let response = client.get(&url).send().await?;
    let data: serde_json::Value = response.json().await?;
    
    if let Some(devs) = data.get("dev_wallets").and_then(|v| v.as_array()) {
        for dev in devs {
            if let Some(addr) = dev.get("creator_address").and_then(|v| v.as_str()) {
                if addr == creator_address {
                    return Ok(true);
                }
            }
        }
    }
    
    Ok(false)
}

// Обновление списка отслеживаемых девов (периодически)
// Автоматически запускает/останавливает снайпер в зависимости от статуса тасков
async fn refresh_tracked_devs(state: AppState) {
    use std::sync::Arc;
    use tokio::sync::Mutex;
    
    let sniper_loop_handle: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let sniper_loop_handle_clone = sniper_loop_handle.clone();
    
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await; // Проверяем каждые 10 секунд
        
        let mut sniper = state.write().await;
        let was_running = sniper.running;
        
        if let Err(e) = load_tracked_devs(&mut sniper).await {
            warn!("⚠️ Error refreshing tracked devs: {}", e);
            continue;
        }
        
        // Проверяем есть ли активный таск
        let has_active_task = sniper.config.is_some() && 
                              sniper.wallet_keypair.is_some() && 
                              !sniper.tracked_devs.is_empty();
        
        // Если есть активный таск и снайпер не запущен - запускаем
        if has_active_task && !was_running {
            info!("🚀 Auto-starting sniper (active task detected)");
            sniper.running = true;
            
            // Запускаем цикл снайпера
            let sniper_state = state.clone();
            let handle = tokio::spawn(async move {
                run_sniper_loop(sniper_state).await;
            });
            
            let mut handle_guard = sniper_loop_handle_clone.lock().await;
            *handle_guard = Some(handle);
        }
        
        // Если нет активного таска и снайпер запущен - останавливаем
        if !has_active_task && was_running {
            info!("🛑 Auto-stopping sniper (no active tasks)");
            sniper.running = false;
            
            // Отменяем задачу снайпера
            let mut handle_guard = sniper_loop_handle_clone.lock().await;
            if let Some(handle) = handle_guard.take() {
                handle.abort();
            }
        }
    }
}

// Покупка токена на PumpFun через RPC
// Использует прямые инструкции PumpFun программы через RPC
async fn buy_token(
    mint_address: &str,
    amount_sol: f64,
    slippage_bps: u16, // Slippage в basis points для расчета min_tokens_out
    wallet: &Keypair,
    priority_fee: f64,
    compute_units: u32,
    creator_address: Option<&str>, // Creator из Create транзакции
    rpc_endpoint: Option<&str>, // RPC endpoint (валидатор/процессор)
    validator_type: &str, // Тип валидатора: "rpc", "jito_bundle", "direct"
    _tip_fee: f64,
    _tip_account_str: &str, // Validator tip account (пока не используется - только для Jito bundles)
    low_latency: bool,
    assume_initial_curve: bool,
    min_tokens_out_override: Option<u64>,
    cached_fee_recipient: Option<Pubkey>,
) -> Result<String> {
    // Используем выбранный RPC endpoint или дефолтный
    let rpc_url = rpc_endpoint
        .or_else(|| Some(RPC_URL))
        .unwrap_or(RPC_URL);
    
    let start_time = std::time::Instant::now();
    if !low_latency {
        info!("🛒 Buying token {} with {} SOL on PumpFun via RPC: {}", mint_address, amount_sol, rpc_url);
    }
    
    let rpc_client = RpcClient::new(rpc_url.to_string());
    
    // Парсим адреса
    let mint_pubkey = Pubkey::from_str(mint_address)
        .context("Invalid mint address")?;
    let pump_fun_program = Pubkey::from_str(PUMP_FUN_PROGRAM_ID)
        .context("Invalid PumpFun program ID")?;
    
    // Находим все необходимые PDA согласно IDL
    let (global_pda, _) = Pubkey::find_program_address(&[b"global"], &pump_fun_program);
    
    // Bonding curve PDA: seeds = ["bonding-curve", mint]
    let (bonding_curve_pubkey, _) = Pubkey::find_program_address(
        &[b"bonding-curve", mint_pubkey.as_ref()],
        &pump_fun_program,
    );
    
    info!("🔍 Bonding curve PDA: {}", bonding_curve_pubkey);
    
    // Получаем creator - либо из параметра (быстрее), либо читаем из bonding curve
    let creator = if let Some(creator_str) = creator_address {
        info!("✅ Using creator from Create transaction: {}", creator_str);
        Pubkey::from_str(creator_str)
            .context("Invalid creator address from Create transaction")?
    } else {
        // Fallback: читаем из bonding curve (может быть медленнее и требует ожидания)
        warn!("⚠️ Creator not provided, reading from bonding curve (may be slower)...");
        let bonding_curve_data = match rpc_client.get_account_data(&bonding_curve_pubkey).await {
            Ok(data) => {
                info!("✅ Bonding curve account found, size: {} bytes", data.len());
                data
            }
            Err(_e) => {
                warn!("⚠️ Bonding curve account not found immediately, waiting 500ms and retrying...");
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                match rpc_client.get_account_data(&bonding_curve_pubkey).await {
                    Ok(data) => {
                        info!("✅ Bonding curve account found after retry, size: {} bytes", data.len());
                        data
                    }
                    Err(_e2) => {
                        warn!("⚠️ Still not found after retry, waiting 1s and trying one more time...");
                        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                        rpc_client
                            .get_account_data(&bonding_curve_pubkey)
                            .await
                            .context(format!("Failed to get bonding curve account after 2 retries. PDA: {}", bonding_curve_pubkey))?
                    }
                }
            }
        };
        
        // BondingCurve структура: discriminator (8) + virtual_token_reserves (8) + virtual_sol_reserves (8) + 
        // real_token_reserves (8) + real_sol_reserves (8) + token_total_supply (8) + complete (1) + creator (32)
        if bonding_curve_data.len() < 81 {
            return Err(anyhow::anyhow!("Invalid bonding curve account data"));
        }
        let creator_bytes = &bonding_curve_data[73..105]; // creator находится с offset 73
        Pubkey::try_from(creator_bytes)
            .context("Failed to parse creator from bonding curve")?
    };
    
    // Associated Token Program и Token Program
    let associated_token_program = Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
        .context("Invalid Associated Token Program ID")?;
    
    // МАКСИМАЛЬНАЯ СКОРОСТЬ: Захардкодим Token-2022 (все токены Pump.fun используют Token-2022)
    // Убираем цикл поиска mint account - это экономит 10-20+ секунд задержки!
    let token_program = Pubkey::from_str(TOKEN_2022_PROGRAM_ID)
        .context("Invalid Token-2022 Program ID")?;
    info!("✅ Assuming Token-2022 Program (Pump.fun standard) - skipping mint account check for speed");
    let system_program = Pubkey::from_str("11111111111111111111111111111111")
        .context("Invalid System Program ID")?;
    
    // Associated Bonding Curve (ATA для bonding curve)
    // МАКСИМАЛЬНАЯ СКОРОСТЬ: Убираем проверки существования - idempotent инструкция безопасна
    let associated_bonding_curve = get_associated_token_address_with_program_id(
        &bonding_curve_pubkey,  // owner = bonding_curve PDA
        &mint_pubkey,           // mint
        &token_program,         // token_program (Token-2022)
    );
    
    info!("🔍 Associated bonding curve ATA: {}", associated_bonding_curve);
    
    let mut instructions = vec![];
    
    // Associated User (ATA для пользователя)
    // МАКСИМАЛЬНАЯ СКОРОСТЬ: Убираем проверки существования - idempotent инструкция безопасна
    // Всегда используем Token-2022 (Pump.fun стандарт)
    let associated_user = get_associated_token_address_with_program_id(
        &wallet.pubkey(), // owner
        &mint_pubkey,     // mint
        &token_program,   // token_program (Token-2022)
    );
    
    info!("🔍 Associated user ATA: {}", associated_user);
    
    // Всегда добавляем idempotent инструкцию создания user ATA (безопасно, если уже существует)
    let create_user_ata_instruction = solana_sdk::instruction::Instruction {
        program_id: associated_token_program,
        accounts: vec![
            solana_sdk::instruction::AccountMeta::new(wallet.pubkey(), true), // payer (writable, signer)
            solana_sdk::instruction::AccountMeta::new(associated_user, false), // ata (writable)
            solana_sdk::instruction::AccountMeta::new_readonly(wallet.pubkey(), false), // owner (readonly)
            solana_sdk::instruction::AccountMeta::new_readonly(mint_pubkey, false), // mint (readonly)
            solana_sdk::instruction::AccountMeta::new_readonly(system_program, false), // system_program
            solana_sdk::instruction::AccountMeta::new_readonly(token_program, false), // Token-2022
        ],
        data: vec![], // Idempotent: пустой data
    };
    instructions.push(create_user_ata_instruction);
    info!("📝 Always adding idempotent create for user ATA (Token-2022)");
    
    // КРИТИЧНО: НЕ создаем bonding curve ATA - Pump.fun создает его автоматически при создании токена
    // Попытка создать его вручную вызывает IllegalOwner ошибку, так как bonding_curve - это PDA
    // associated_bonding_curve адрес уже рассчитан выше и будет использован в buy instruction
    
    // Creator Vault
    let (creator_vault, _) = Pubkey::find_program_address(
        &[b"creator-vault", creator.as_ref()],
        &pump_fun_program,
    );
    
    // Event Authority
    let (event_authority, _) = Pubkey::find_program_address(
        &[b"__event_authority"],
        &pump_fun_program,
    );
    
    // Global Volume Accumulator
    let (global_volume_accumulator, _) = Pubkey::find_program_address(
        &[b"global_volume_accumulator"],
        &pump_fun_program,
    );
    
    // User Volume Accumulator
    let (user_volume_accumulator, _) = Pubkey::find_program_address(
        &[b"user_volume_accumulator", wallet.pubkey().as_ref()],
        &pump_fun_program,
    );
    
    // Fee Program и Fee Config
    let fee_program = Pubkey::from_str("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ")
        .context("Invalid Fee Program ID")?;
    let fee_config_seed = [
        1, 86, 224, 246, 147, 102, 90, 207, 68, 219, 21, 104, 191, 23, 91, 170,
        81, 137, 203, 151, 245, 210, 255, 59, 101, 93, 43, 182, 253, 109, 24, 176,
    ];
    let (fee_config, _) = Pubkey::find_program_address(
        &[b"fee_config", &fee_config_seed],
        &fee_program,
    );
    
    // FIX: Кешируем fee_recipient для скорости (статический)
    let fee_recipient = if let Some(cached) = cached_fee_recipient {
        cached
    } else {
        // Читаем global account для получения fee_recipient (только если не закеширован)
        let global_data = rpc_client
            .get_account_data(&global_pda)
            .await
            .context("Failed to get global account")?;
        
        // Global структура: discriminator (8) + initialized (1) + authority (32) + fee_recipient (32) + ...
        // fee_recipient находится с offset 41 (8 + 1 + 32)
        if global_data.len() >= 73 {
            Pubkey::try_from(&global_data[41..73])
                .context("Failed to parse fee_recipient from global")?
        } else {
            wallet.pubkey() // Fallback
        }
    };
    
    // FIX: Используем кешированный blockhash для скорости (обновляется в фоне каждые 2s)
    // Для low_latency используем кеш, иначе получаем свежий
    let recent_blockhash = if low_latency {
        // Используем кеш из state (обновляется в фоне каждые 2s)
        // Если кеш пуст, получаем свежий
        // TODO: передать state в buy_token для доступа к кешу
        rpc_client
            .get_latest_blockhash()
            .await
            .context("Failed to get latest blockhash")?
    } else {
        rpc_client
            .get_latest_blockhash()
            .await
            .context("Failed to get latest blockhash")?
    };
    
    let amount_lamports = (amount_sol * 1e9) as u64;
    
    // Priority Fee: total SOL from config -> price per CU (microlamports per CU)
    // FIX: set_compute_unit_price expects micro-lamports per CU, not total lamports!
    // Formula: price_micro_per_cu = (total_priority_lamports * 1_000_000) / compute_units
    let total_priority_lamports = (priority_fee * 1e9) as u64; // 0.1 SOL -> 100_000_000 lamports
    let price_micro_per_cu = if compute_units > 0 {
        (total_priority_lamports * 1_000_000) / compute_units as u64
    } else {
        0
    };
    
    // Optional: Dynamic fee adjustment from recent prioritization fees
    let final_price_micro_per_cu = match rpc_client.get_recent_prioritization_fees(&[]).await {
        Ok(fees) => {
            if !fees.is_empty() {
                let max_recent = fees.iter().map(|f| f.prioritization_fee).max().unwrap_or(0);
                // Use max of recent fees or user's priority fee
                price_micro_per_cu.max(max_recent)
            } else {
                price_micro_per_cu
            }
        }
        Err(e) => {
            warn!("⚠️ Failed to get recent prioritization fees: {}, using config value", e);
            price_micro_per_cu
        }
    };
    
    info!("💰 Priority: {} micro/CU (total ~{} SOL)", final_price_micro_per_cu, priority_fee);
    
    // FIX: Для 0-1 block speed - set min_tokens_out = 1 (bypass slippage, avoid 6020 zero amount error)
    let min_tokens_out = if low_latency {
        info!("⚡ Set min_out=1 to bypass zero amount error (6020) - no slippage check");
        1u64 // Min 1 token to avoid 6020, accept low amounts for speed
    } else if assume_initial_curve {
        // Assumed initial curve state: virtual_token_reserves = 800M tokens * 1e6 decimals = 8e11
        let virtual_token_reserves = 800_000_000_000_000u64; // Initial from Pump.fun docs
        let virtual_sol_reserves = 0u64; // Initial state
        
        let tokens_out = if virtual_sol_reserves + amount_lamports > 0 {
            ((amount_lamports as u128)
                .checked_mul(virtual_token_reserves as u128)
                .and_then(|x| x.checked_div((virtual_sol_reserves + amount_lamports) as u128))
                .unwrap_or(virtual_token_reserves as u128)) as u64
        } else {
            virtual_token_reserves
        };
        
        // Если override задан, используем его (0 = no check)
        if let Some(override_val) = min_tokens_out_override {
            if override_val == 0 {
                0 // No slippage check for max speed
            } else {
                override_val
            }
        } else {
            let slippage_factor = 1.0 - (slippage_bps as f64 / 10000.0);
            (tokens_out as f64 * slippage_factor * 0.5) as u64 // Conservative: /2 для безопасности
        }
    } else {
        // Старый код с RPC read (для non-low-latency режима)
        let mut attempts = 0;
        let max_attempts = 10;
        
        loop {
            match rpc_client.get_account_data(&bonding_curve_pubkey).await {
                Ok(data) => {
                    if data.len() >= 24 {
                        let virtual_token_reserves = u64::from_le_bytes(
                            data[8..16].try_into().unwrap_or([0; 8])
                        );
                        let virtual_sol_reserves = u64::from_le_bytes(
                            data[16..24].try_into().unwrap_or([0; 8])
                        );
                        
                        if virtual_sol_reserves > 0 && virtual_token_reserves > 0 {
                            let tokens_out = ((amount_lamports as u128)
                                .checked_mul(virtual_token_reserves as u128)
                                .and_then(|x| x.checked_div((virtual_sol_reserves + amount_lamports) as u128))
                                .unwrap_or(0)) as u64;
                            
                            let slippage_factor = 1.0 - (slippage_bps as f64 / 10000.0);
                            let min_tokens_out = (tokens_out as f64 * slippage_factor) as u64;
                            
                            if !low_latency {
                                info!("💰 Calculated: tokens_out {}, min_tokens_out {} (slippage {} bps)", 
                                      tokens_out, min_tokens_out, slippage_bps);
                            }
                            
                            break min_tokens_out.max(1);
                        } else {
                            attempts += 1;
                            if attempts >= max_attempts {
                                return Err(anyhow::anyhow!("Bonding curve not ready — virtual reserves are zero (timeout after {} attempts)", max_attempts));
                            }
                            let sleep_ms = 50 * (1 << attempts.min(6));
                            tokio::time::sleep(tokio::time::Duration::from_millis(sleep_ms)).await;
                        }
                    } else {
                        attempts += 1;
                        if attempts >= max_attempts {
                            return Err(anyhow::anyhow!("Bonding curve data invalid — timeout after {} attempts", max_attempts));
                        }
                        let sleep_ms = 50 * (1 << attempts.min(6));
                        tokio::time::sleep(tokio::time::Duration::from_millis(sleep_ms)).await;
                    }
                }
                Err(_) => {
                    attempts += 1;
                    if attempts >= max_attempts {
                        return Err(anyhow::anyhow!("Bonding curve not ready — timeout after {} attempts", max_attempts));
                    }
                    let sleep_ms = 50 * (1 << attempts.min(6));
                    tokio::time::sleep(tokio::time::Duration::from_millis(sleep_ms)).await;
                }
            }
        }
    };
    
    if !low_latency {
        info!("💰 Using buy_exact_sol_in: spendable_sol_in {} lamports ({:.9} SOL), min_tokens_out {}", 
              amount_lamports, amount_sol, min_tokens_out);
    }
    
    // Discriminator для buy_exact_sol_in: [56, 252, 116, 8, 158, 223, 205, 95]
    let mut instruction_data = vec![56, 252, 116, 8, 158, 223, 205, 95];
    instruction_data.extend_from_slice(&amount_lamports.to_le_bytes()); // spendable_sol_in: u64 - фиксированное количество SOL
    instruction_data.extend_from_slice(&min_tokens_out.to_le_bytes()); // min_tokens_out: u64 - минимальное количество токенов с учетом slippage
    instruction_data.push(0); // track_volume: 0 = false (u8)
    
    // Создаем инструкцию ComputeBudget для установки приорити фии и комп юнитов
    use solana_compute_budget_interface::ComputeBudgetInstruction;
    let compute_budget_instruction = ComputeBudgetInstruction::set_compute_unit_price(final_price_micro_per_cu);
    let compute_unit_limit_instruction = ComputeBudgetInstruction::set_compute_unit_limit(compute_units);
    
    
    // Создаем инструкцию buy для PumpFun
    // Правильный порядок accounts согласно актуальному IDL Pump.fun:
    // 0. global (readonly)
    // 1. fee_recipient (writable)
    // 2. mint (readonly)
    // 3. bonding_curve (writable)
    // 4. associated_bonding_curve (writable)
    // 5. associated_user (writable)
    // 6. user (writable, signer)
    // 7. system_program (readonly)
    // 8. token_program (readonly) - стандартный Token Program
    // 9. creator_vault (writable) - КРИТИЧНО: должен быть на позиции 9, не 11!
    // 10. event_authority (readonly)
    // 11. program (readonly) - КРИТИЧНО: должен быть на позиции 11 для Anchor проверки
    // 12. global_volume_accumulator (writable)
    // 13. user_volume_accumulator (writable)
    // 14. fee_config (readonly)
    // 15. fee_program (readonly)
    // КРИТИЧНО: В IDL buy нет rent sysvar - убираем его
    // КРИТИЧНО: token_program в buy instruction должен быть Token-2022 (Pump.fun использует Token-2022 для всех токенов)
    // ATA creation также использует Token-2022, так как mint owner - Token-2022
    
    let buy_instruction = solana_sdk::instruction::Instruction {
        program_id: pump_fun_program,
        accounts: vec![
            solana_sdk::instruction::AccountMeta::new_readonly(global_pda, false), // 0: global (readonly)
            solana_sdk::instruction::AccountMeta::new(fee_recipient, false), // 1: fee_recipient (writable)
            solana_sdk::instruction::AccountMeta::new_readonly(mint_pubkey, false), // 2: mint (readonly)
            solana_sdk::instruction::AccountMeta::new(bonding_curve_pubkey, false), // 3: bonding_curve (writable)
            solana_sdk::instruction::AccountMeta::new(associated_bonding_curve, false), // 4: associated_bonding_curve (writable)
            solana_sdk::instruction::AccountMeta::new(associated_user, false), // 5: associated_user (writable)
            solana_sdk::instruction::AccountMeta::new(wallet.pubkey(), true), // 6: user (writable, signer)
            solana_sdk::instruction::AccountMeta::new_readonly(system_program, false), // 7: system_program (readonly)
            solana_sdk::instruction::AccountMeta::new_readonly(token_program, false), // 8: token_program (readonly) - Token-2022 (Pump.fun использует Token-2022)
            solana_sdk::instruction::AccountMeta::new(creator_vault, false), // 9: creator_vault (writable) - ИСПРАВЛЕНО: перемещен с позиции 11 на 9
            solana_sdk::instruction::AccountMeta::new_readonly(event_authority, false), // 10: event_authority (readonly) - ИСПРАВЛЕНО: перемещен с позиции 9 на 10
            solana_sdk::instruction::AccountMeta::new_readonly(pump_fun_program, false), // 11: program (readonly) - ИСПРАВЛЕНО: перемещен с позиции 10 на 11 (Anchor требует это)
            solana_sdk::instruction::AccountMeta::new(global_volume_accumulator, false), // 12: global_volume_accumulator (writable) - для volume tracking
            solana_sdk::instruction::AccountMeta::new(user_volume_accumulator, false), // 13: user_volume_accumulator (writable) - для volume tracking
            solana_sdk::instruction::AccountMeta::new_readonly(fee_config, false), // 14: fee_config (readonly) - для fee configuration
            solana_sdk::instruction::AccountMeta::new_readonly(fee_program, false), // 15: fee_program (readonly) - для fee program
        ],
        data: instruction_data,
    };
    
    // Создаем транзакцию с ComputeBudget инструкциями
    // Если нужно создать associated accounts, добавляем инструкции создания перед buy
    // По логам успешных транзакций, порядок должен быть:
    // 1. ComputeBudget (2 раза)
    // 2. Associated Token Program: CreateIdempotent (если account не существует)
    // 3. PumpFun: Buy
    let transaction_instructions = vec![
        compute_budget_instruction,
        compute_unit_limit_instruction,
    ];
    
    // Добавляем инструкции создания associated accounts, если нужно
    // Сначала associated_user (для пользователя), потом associated_bonding_curve (если нужно)
    transaction_instructions.extend(instructions);
    
    // Добавляем buy инструкцию
    transaction_instructions.push(buy_instruction);
    
    // FIX: Tip работает только в Jito bundles, для обычного RPC это бесполезно
    // Валидаторы игнорируют tip в обычных транзакциях
    // TODO: Реализовать Jito bundle API если нужен tip
    // if tip_fee > 0.0 && is_jito_bundle {
    //     let tip_pubkey = Pubkey::from_str(tip_account_str)
    //         .context("Invalid tip account pubkey")?;
    //     let tip_lamports = (tip_fee * 1e9) as u64;
    //     let tip_instruction = system_instruction::transfer(
    //         &wallet.pubkey(),
    //         &tip_pubkey,
    //         tip_lamports,
    //     );
    //     transaction_instructions.push(tip_instruction);
    //     info!("💸 Added validator tip: {} SOL to {} (Jito bundle)", tip_fee, tip_account_str);
    // }
    
    let mut transaction = Transaction::new_with_payer(
        &transaction_instructions,
        Some(&wallet.pubkey()),
    );
    
    transaction.sign(&[wallet], recent_blockhash);
    
    // Логируем полную транзакцию для отладки (base58 encoded)
    let serialized = bincode::serialize(&transaction)
        .unwrap_or_else(|_| vec![]);
    if !serialized.is_empty() {
        let tx_base58 = bs58::encode(&serialized).into_string();
        info!("📝 Transaction base58 (for explorer): {}", tx_base58);
    }
    
    if !low_latency {
        info!("📝 Transaction created: {} lamports, priority fee: {} SOL total ({} micro/CU), compute units: {}", 
              amount_lamports, priority_fee, final_price_micro_per_cu, compute_units);
    }
    
    // FIX: Для low_latency режима - skip simulation (экономит 30ms+)
    let sim_success = if low_latency {
        true // Skip sim
    } else {
        // Старый код с simulation (для non-low-latency режима)
        info!("🔍 Simulating transaction before send...");
        use solana_client::rpc_config::RpcSimulateTransactionConfig;
        use solana_sdk::commitment_config::CommitmentConfig;
        
        let mut sim_success = false;
        for _attempt in 0..1 { // Уменьшено до 1 attempt для скорости
            let sim_config = RpcSimulateTransactionConfig {
                sig_verify: false,
                replace_recent_blockhash: false,
                commitment: Some(CommitmentConfig::processed()),
                encoding: None,
                accounts: None,
                min_context_slot: None,
                inner_instructions: true,
            };
            
            match rpc_client.simulate_transaction_with_config(&transaction, sim_config).await {
                Ok(sim_result) => {
                    if let Some(err) = sim_result.value.err {
                        if let TransactionError::InstructionError(_, InstructionError::Custom(code)) = err {
                            if code == 3005 || code == 3007 || code == 3008 || code == 2023 {
                                warn!("⚠️ Ignoring sim error Custom({}) — sending on-chain anyway", code);
                                sim_success = true;
                                break;
                            }
                        }
                        warn!("❌ Simulation failed: {:?} — sending anyway", err);
                        sim_success = true; // Send anyway for speed
                        break;
                    }
                    sim_success = true;
                    break;
                }
                Err(e) => {
                    warn!("⚠️ Simulation error: {} — sending anyway", e);
                    sim_success = true; // Send anyway
                    break;
                }
            }
        }
        sim_success
    };
    
    if !sim_success {
        warn!("⚠️ Simulation failed, sending anyway for speed");
    }
    
    // FIX: sim_only mode - только симуляция, не отправка
    // Проверяем через параметр функции или config
    // Для простоты добавим параметр в buy_token позже, пока используем config
    
    // Отправляем транзакцию в зависимости от типа валидатора
    if !low_latency {
        info!("📤 Sending transaction via {} to: {}", validator_type, rpc_url);
    }
    
    let config = RpcSendTransactionConfig {
        skip_preflight: false, // Можно поставить true для максимальной скорости, но симуляция уже сделана
        preflight_commitment: None,
        max_retries: Some(3),
        min_context_slot: None,
        encoding: None,
    };
    
    // TODO: Реализовать разные типы отправки:
    // - "jito_bundle": отправка bundle через Jito API
    // - "direct": прямая отправка валидатору
    // Пока все типы используют стандартный RPC
    if validator_type == "jito_bundle" {
        warn!("⚠️ Jito bundle submission not yet implemented, using RPC instead");
    } else if validator_type == "direct" {
        warn!("⚠️ Direct validator submission not yet implemented, using RPC instead");
    }
    
    let signature = match rpc_client
        .send_transaction_with_config(&transaction, config)
        .await
    {
        Ok(sig) => {
            if !low_latency {
                info!("✅ Transaction sent successfully: {}", sig);
            }
            sig
        }
        Err(e) => {
            error!("❌ Failed to send transaction: {}", e);
            error!("   Transaction details:");
            error!("   - Amount: {} lamports", amount_lamports);
            error!("   - Priority fee: {} SOL total ({} micro/CU)", priority_fee, final_price_micro_per_cu);
            error!("   - Compute units: {}", compute_units);
            error!("   - Wallet: {}", wallet.pubkey());
            error!("   - Mint: {}", mint_address);
            error!("   - Validator type: {}", validator_type);
            return Err(anyhow::anyhow!("Failed to send transaction: {}", e));
        }
    };
    
    // FIX: Для low_latency - уменьшено attempts и sleep для скорости
    if !low_latency {
        info!("🔍 Checking transaction status...");
    }
    
    // Ждем немного перед проверкой (транзакция должна попасть в блокчейн)
    if !low_latency {
        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;
    } else {
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await; // Минимальная задержка
    }
    
    // Проверяем статус с несколькими попытками
    let mut confirmed = false;
    let mut failed = false;
    let mut error_msg = None;
    
    // FIX: Для low_latency - уменьшено attempts и sleep для скорости
    let max_status_attempts = if low_latency { 5 } else { 15 };
    let status_sleep_ms = if low_latency { 200 } else { 400 };
    
    for attempt in 1..=max_status_attempts {
        match rpc_client.get_signature_status(&signature).await {
            Ok(Some(status_result)) => {
                match status_result {
                    Ok(_) => {
                        confirmed = true;
                        info!("✅ Transaction CONFIRMED! Signature: {}", signature);
                        break;
                    }
                    Err(err) => {
                        error_msg = Some(format!("{:?}", err));
                        failed = true;
                        error!("❌ Transaction FAILED: {:?}", err);
                        break;
                    }
                }
            }
            Ok(None) => {
                if attempt < max_status_attempts {
                    if !low_latency && attempt % 5 == 0 {
                        info!("⏳ Transaction pending, waiting... (attempt {}/{})", attempt, max_status_attempts);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(status_sleep_ms)).await;
                } else {
                    warn!("⚠️ Transaction status not found after {} attempts - likely dropped", max_status_attempts);
                    error_msg = Some("Dropped — low fee or congestion".to_string());
                }
            }
            Err(e) => {
                if attempt < max_status_attempts {
                    if !low_latency && attempt % 5 == 0 {
                        warn!("⚠️ Failed to get transaction status (attempt {}): {}, retrying...", attempt, e);
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(status_sleep_ms)).await;
                } else {
                    error!("❌ Failed to get transaction status after {} attempts: {}", max_status_attempts, e);
                }
            }
        }
    }
    
    // Если все еще не подтверждено, пробуем получить транзакцию напрямую
    if !confirmed && !failed {
        info!("🔍 Trying to get transaction directly...");
        match rpc_client.get_transaction(&signature, UiTransactionEncoding::Json).await {
            Ok(tx) => {
                if let Some(meta) = tx.transaction.meta {
                    if let Some(err) = meta.err {
                        error_msg = Some(format!("{:?}", err));
                        failed = true;
                        error!("❌ Transaction FAILED (from get_transaction): {:?}", err);
                    } else {
                        confirmed = true;
                        info!("✅ Transaction CONFIRMED (from get_transaction)! Signature: {}", signature);
                    }
                } else {
                    // Если нет meta, но транзакция найдена, считаем успешной
                    confirmed = true;
                    info!("✅ Transaction CONFIRMED (from get_transaction, no meta)! Signature: {}", signature);
                }
            }
            Err(e) => {
                warn!("⚠️ Failed to get transaction: {}", e);
            }
        }
    }
    
    let elapsed = start_time.elapsed();
    
    if confirmed {
        info!("✅ Transaction CONFIRMED! Signature: {} (time: {:?})", signature, elapsed);
    } else if failed {
        error!("❌ Transaction FAILED! Signature: {} (time: {:?})", signature, elapsed);
        if let Some(ref err) = error_msg {
            error!("   Error: {}", err);
        }
        return Err(anyhow::anyhow!("Transaction failed: {}", error_msg.unwrap_or_else(|| "Unknown error".to_string())));
    } else {
        error!("❌ Transaction NOT FOUND in blockchain! Signature: {}", signature);
        error!("   This usually means the transaction was rejected or dropped by the network");
        return Err(anyhow::anyhow!("Transaction not found in blockchain - likely rejected or dropped"));
    }
    
    Ok(signature.to_string())
}

// Обработка Create транзакции
async fn handle_create_transaction(
    create_tx: CreateTransaction,
    state: AppState,
) -> Result<()> {
    info!("🔍 Processing Create transaction:");
    info!("   - Creator: {}", create_tx.creator_address);
    info!("   - Mint: {}", create_tx.mint_address);
    info!("   - Signature: {}", create_tx.signature);
    
    // Получаем данные из состояния
    let (tracked_devs, config) = {
        let sniper = state.read().await;
        (
            sniper.tracked_devs.clone(),
            sniper.config.clone(),
        )
    };
    
    // ОБРАБАТЫВАЕМ ТОЛЬКО ОДИН ТОКЕН - проверяем флаг token_purchased
    {
        let sniper = state.read().await;
        if sniper.token_purchased {
            debug!("⏭️ Token already purchased in this session, skipping {}", create_tx.mint_address);
            return Ok(());
        }
        if sniper.wallet_keypair.is_none() {
            error!("❌ Wallet keypair not configured");
            return Ok(());
        }
    }
    
    // Проверяем, отслеживаем ли мы этого дева
    if !tracked_devs.contains(&create_tx.creator_address) {
        debug!("⏭️ Creator {} not in tracked list ({} tracked), skipping", create_tx.creator_address, tracked_devs.len());
        return Ok(());
    }
    
    info!("✅ Creator {} is in tracked list! Proceeding with purchase...", create_tx.creator_address);
    info!("🎯 Found token from tracked dev: {} -> {}", 
          create_tx.creator_address, create_tx.mint_address);
    
    // Если адрес в списке отслеживаемых - это кастомный адрес, покупаем без проверки в датабазе
    // Проверка в датабазе нужна только для дева из списка dev_wallets в таске
    // Кастомные адреса уже добавлены в tracked_devs, поэтому просто покупаем
    
    // Проверяем конфигурацию
    let config = match config {
        Some(cfg) if cfg.auto_buy => cfg,
        _ => {
            warn!("⚠️ Auto-buy disabled or config missing");
            return Ok(());
        }
    };
    
    // Обновляем статистику
    {
        let mut sniper = state.write().await;
        sniper.stats.tokens_created += 1;
    }
    
    // Покупаем токен
    let slippage_bps = (config.slippage * 100.0) as u16;
    
    let priority_fee = config.priority_fee;
    let compute_units = config.compute_units; // Из конфига таска
    
    if !config.low_latency {
        info!("🎯 Attempting to buy token: {}", create_tx.mint_address);
        info!("   Amount: {} SOL", config.amount);
        info!("   Slippage: {}%", config.slippage);
        info!("   Priority fee: {} SOL", priority_fee);
        info!("   Compute units: {}", compute_units);
        info!("   Creator: {}", create_tx.creator_address);
    }
    
    // Вызываем buy_token с retry логикой для ошибки 6020 (Buy zero amount)
    let mut retry_amount = config.amount;
    let mut buy_result = None;
    let mut retry_count = 0;
    let max_retries = 2;
    
    while retry_count <= max_retries {
        let sniper = state.read().await;
        let wallet = match sniper.wallet_keypair.as_ref() {
            Some(k) => k,
            None => {
                error!("❌ Wallet keypair not configured");
                return Ok(());
            }
        };
        
        let cached_fee = {
            let sniper_read = state.read().await;
            sniper_read.cached_fee_recipient
        };
        
        let validator_type = config.validator_type.as_deref().unwrap_or("rpc");
        let purchase_validator = config.purchase_validator.as_deref();
        
        let result = buy_token(
            &create_tx.mint_address,
            retry_amount,
            slippage_bps,
            wallet,
            priority_fee,
            compute_units,
            Some(&create_tx.creator_address),
            purchase_validator,
            validator_type,
            config.tip_fee,
            &config.tip_account,
            config.low_latency,
            config.assume_initial_curve,
            config.min_tokens_out_override,
            cached_fee,
        ).await;
        
        let error_str = result.as_ref().err().map(|e| e.to_string());
        let is_6020 = error_str.as_ref().map(|s| s.contains("6020") || s.contains("Buy zero amount") || s.contains("zero amount")).unwrap_or(false);
        
        if result.is_ok() || !is_6020 || retry_count >= max_retries {
            buy_result = Some(result);
            break;
        }
        
        // Retry с увеличенным amount
        retry_count += 1;
        retry_amount *= 1.5;
        warn!("⚠️ Zero amount error (6020) - retrying with {} SOL (attempt {}/{})", retry_amount, retry_count, max_retries);
        drop(sniper); // Release lock before retry
    }
    
    let buy_result = buy_result.expect("buy_result should be set");
    
    match buy_result {
        Ok(signature) => {
            info!("✅ Successfully bought token!");
            info!("   Signature: {}", signature);
            info!("   Mint: {}", create_tx.mint_address);
            info!("   Creator: {}", create_tx.creator_address);
            if retry_count > 0 {
                info!("   Final amount: {} SOL (retried {} times)", retry_amount, retry_count);
            }
            let mut sniper = state.write().await;
            sniper.stats.tokens_sniped += 1;
            sniper.stats.successful += 1;
            sniper.stats.transactions_sent += 1;
            sniper.token_purchased = true; // Устанавливаем флаг: токен куплен, больше не обрабатываем другие
            info!("🔒 Token purchased flag set - will skip other tokens in this session");
            
            // Создаем позицию для мониторинга
            if let Some(ref cfg) = sniper.config {
                if cfg.auto_sell_enabled {
                    // Получаем количество токенов из транзакции
                    let held = {
                        let rpc = RpcClient::new(RPC_URL.to_string());
                        let sig_bytes = bs58::decode(&signature).into_vec().ok()
                            .and_then(|bytes| solana_sdk::signature::Signature::try_from(bytes.as_slice()).ok());
                        if let Some(sig) = sig_bytes {
                            match rpc.get_transaction(&sig, UiTransactionEncoding::JsonParsed).await {
                                Ok(tx) => {
                                    if let Some(meta) = tx.transaction.meta {
                                        // Пытаемся извлечь количество токенов из inner_instructions
                                        // Ищем TransferChecked инструкцию в последней inner instruction
                                        let inner_instructions: Vec<_> = meta.inner_instructions.into();
                                        if let Some(last_inner) = inner_instructions.last() {
                                            if let Some(transfer_ix) = last_inner.instructions.last() {
                                                // Декодируем data из base58
                                                if let Ok(data_bytes) = bs58::decode(&transfer_ix.data).into_vec() {
                                                    // TransferChecked: discriminator (1 byte) + amount (8 bytes)
                                                    // Проверяем что это TransferChecked (discriminator = 12)
                                                    if data_bytes.len() >= 12 && data_bytes[0] == 12 {
                                                        let amount_bytes = &data_bytes[data_bytes.len() - 8..];
                                                        u64::from_le_bytes(amount_bytes.try_into().unwrap_or([0; 8]))
                                                    } else {
                                                        0
                                                    }
                                                } else {
                                                    0
                                                }
                                            } else {
                                                0
                                            }
                                        } else {
                                            0
                                        }
                                    } else {
                                        0
                                    }
                                }
                                Err(_) => 0,
                            }
                        } else {
                            0
                        }
                    };
                    
                    let mint_pubkey = Pubkey::from_str(&create_tx.mint_address).ok();
                    let pump_fun_program = Pubkey::from_str(PUMP_FUN_PROGRAM_ID).ok();
                    if let (Some(mint), Some(program)) = (mint_pubkey, pump_fun_program) {
                        let (bonding_curve, _) = Pubkey::find_program_address(
                            &[b"bonding-curve", mint.as_ref()],
                            &program,
                        );
                        
                        let position = Position {
                            mint: create_tx.mint_address.clone(),
                            bonding_curve,
                            held_tokens: held,
                            buy_sol: retry_amount,
                        };
                        let pos_idx = sniper.positions.len();
                        sniper.positions.push(position);
                        info!("📊 Position added for monitoring: {} (idx: {})", create_tx.mint_address, pos_idx);
                        
                        // Запускаем мониторинг позиции
                        let state_clone = state.clone();
                        tokio::spawn(async move {
                            monitor_position(state_clone, pos_idx).await;
                        });
                    }
                }
            }
        }
        Err(e) => {
            let error_str = e.to_string();
            let is_dropped = error_str.contains("Dropped") || error_str.contains("not found");
            
            error!("❌ Failed to buy token!");
            error!("   Mint: {}", create_tx.mint_address);
            error!("   Creator: {}", create_tx.creator_address);
            error!("   Amount: {} SOL", retry_amount);
            error!("   Error: {}", e);
            
            // FIX: Если dropped, не устанавливаем token_purchased = true, можно retry с увеличенным fee
            if is_dropped {
                warn!("⚠️ Transaction dropped - will retry with higher fee if enabled");
                // TODO: Реализовать retry с priority_fee *= 2.0 (до 3 раз)
            }
            
            let mut sniper = state.write().await;
            sniper.stats.failed += 1;
            sniper.stats.errors += 1;
            sniper.stats.transactions_sent += 1;
            
            // FIX: Если dropped, не блокируем покупку других токенов
            if !is_dropped {
                sniper.token_purchased = true; // Только для реальных ошибок, не для drops
            }
        }
    }
    
    Ok(())
}

// Продажа токена на PumpFun
async fn sell_token(
    mint_address: &str,
    tokens_in: u64,
    min_sol_out: f64,
    wallet: &Keypair,
    priority_fee: f64,
    compute_units: u32,
) -> Result<String> {
    let rpc_client = RpcClient::new(RPC_URL.to_string());
    let mint_pubkey = Pubkey::from_str(mint_address)
        .context("Invalid mint address")?;
    let pump_fun_program = Pubkey::from_str(PUMP_FUN_PROGRAM_ID)
        .context("Invalid PumpFun program ID")?;
    let (bonding_curve_pubkey, _) = Pubkey::find_program_address(
        &[b"bonding-curve", mint_pubkey.as_ref()],
        &pump_fun_program,
    );
    let token_program = Pubkey::from_str(TOKEN_2022_PROGRAM_ID)
        .context("Invalid Token-2022 Program ID")?;
    let associated_bonding_curve = get_associated_token_address_with_program_id(
        &bonding_curve_pubkey,
        &mint_pubkey,
        &token_program,
    );
    let associated_user = get_associated_token_address_with_program_id(
        &wallet.pubkey(),
        &mint_pubkey,
        &token_program,
    );
    let (global_pda, _) = Pubkey::find_program_address(&[b"global"], &pump_fun_program);
    
    // Читаем fee_recipient из global account
    let global_data = rpc_client
        .get_account_data(&global_pda)
        .await
        .context("Failed to get global account")?;
    let fee_recipient = if global_data.len() >= 73 {
        Pubkey::try_from(&global_data[41..73])
            .context("Failed to parse fee_recipient from global")?
    } else {
        wallet.pubkey()
    };
    
    let (creator_vault, _) = Pubkey::find_program_address(
        &[b"creator-vault", fee_recipient.as_ref()],
        &pump_fun_program,
    );
    let (event_authority, _) = Pubkey::find_program_address(
        &[b"__event_authority"],
        &pump_fun_program,
    );
    let (global_volume_accumulator, _) = Pubkey::find_program_address(
        &[b"global_volume_accumulator"],
        &pump_fun_program,
    );
    let (user_volume_accumulator, _) = Pubkey::find_program_address(
        &[b"user_volume_accumulator", wallet.pubkey().as_ref()],
        &pump_fun_program,
    );
    let fee_program = Pubkey::from_str("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ")
        .context("Invalid Fee Program ID")?;
    let fee_config_seed = [
        1, 86, 224, 246, 147, 102, 90, 207, 68, 219, 21, 104, 191, 23, 91, 170,
        81, 137, 203, 151, 245, 210, 255, 59, 101, 93, 43, 182, 253, 109, 24, 176,
    ];
    let (fee_config, _) = Pubkey::find_program_address(
        &[b"fee_config", &fee_config_seed],
        &fee_program,
    );
    let system_program = Pubkey::from_str("11111111111111111111111111111111")
        .context("Invalid System Program ID")?;
    let recent_blockhash = rpc_client
        .get_latest_blockhash()
        .await
        .context("Failed to get latest blockhash")?;
    
    let total_priority_lamports = (priority_fee * 1e9) as u64;
    let price_micro_per_cu = if compute_units > 0 {
        (total_priority_lamports * 1_000_000) / compute_units as u64
    } else {
        0
    };
    
    use solana_compute_budget_interface::ComputeBudgetInstruction;
    let compute_budget_instruction = ComputeBudgetInstruction::set_compute_unit_price(price_micro_per_cu);
    let compute_unit_limit_instruction = ComputeBudgetInstruction::set_compute_unit_limit(compute_units);
    
    let min_sol_lamports = (min_sol_out * 1e9) as u64;
    // Discriminator для sell: [157, 172, 117, 171, 172, 29, 38, 206]
    let mut instruction_data = vec![157, 172, 117, 171, 172, 29, 38, 206];
    instruction_data.extend_from_slice(&tokens_in.to_le_bytes()); // tokens_in
    instruction_data.extend_from_slice(&min_sol_lamports.to_le_bytes()); // min_sol_out
    instruction_data.push(0); // track_volume
    
    let sell_instruction = solana_sdk::instruction::Instruction {
        program_id: pump_fun_program,
        accounts: vec![
            solana_sdk::instruction::AccountMeta::new_readonly(global_pda, false),
            solana_sdk::instruction::AccountMeta::new(fee_recipient, false),
            solana_sdk::instruction::AccountMeta::new_readonly(mint_pubkey, false),
            solana_sdk::instruction::AccountMeta::new(bonding_curve_pubkey, false),
            solana_sdk::instruction::AccountMeta::new(associated_bonding_curve, false),
            solana_sdk::instruction::AccountMeta::new(associated_user, false),
            solana_sdk::instruction::AccountMeta::new(wallet.pubkey(), true),
            solana_sdk::instruction::AccountMeta::new_readonly(system_program, false),
            solana_sdk::instruction::AccountMeta::new_readonly(token_program, false),
            solana_sdk::instruction::AccountMeta::new(creator_vault, false),
            solana_sdk::instruction::AccountMeta::new_readonly(event_authority, false),
            solana_sdk::instruction::AccountMeta::new_readonly(pump_fun_program, false),
            solana_sdk::instruction::AccountMeta::new(global_volume_accumulator, false),
            solana_sdk::instruction::AccountMeta::new(user_volume_accumulator, false),
            solana_sdk::instruction::AccountMeta::new_readonly(fee_config, false),
            solana_sdk::instruction::AccountMeta::new_readonly(fee_program, false),
        ],
        data: instruction_data,
    };
    
    let transaction_instructions = vec![
        compute_budget_instruction,
        compute_unit_limit_instruction,
        sell_instruction,
    ];
    
    let mut transaction = Transaction::new_with_payer(
        &transaction_instructions,
        Some(&wallet.pubkey()),
    );
    transaction.sign(&[wallet], recent_blockhash);
    
    let config = RpcSendTransactionConfig {
        skip_preflight: false,
        preflight_commitment: None,
        max_retries: Some(3),
        min_context_slot: None,
        encoding: None,
    };
    
    let sig = rpc_client
        .send_transaction_with_config(&transaction, config)
        .await
        .context("Failed to send sell transaction")?;
    
    info!("✅ Sold position! Signature: {}", sig);
    Ok(sig.to_string())
}

// Мониторинг одной позиции через GRPC или RPC fallback
async fn monitor_position(state: AppState, idx: usize) {
    let (config, grpc_client) = {
        let s = state.read().await;
        let config = s.config.clone();
        let grpc_client = s.grpc_client.clone();
        (config, grpc_client)
    };
    
    if config.is_none() {
        return;
    }
    
    let config = config.unwrap();
    
    if let Some(grpc) = grpc_client {
        let curve_str = {
            let s = state.read().await;
            if idx >= s.positions.len() {
                return;
            }
            s.positions[idx].bonding_curve.to_string()
        };
        
        // Клонируем Arc и создаем mutable reference
        let mut grpc_clone = (*grpc).clone();
        
        match grpc_clone.subscribe_to_account(&curve_str).await {
            Ok(mut stream) => {
                while let Some(update_result) = stream.next().await {
                    match update_result {
                        Ok(up) => {
                            // Парсим данные из GRPC update
                            if let Some(update_oneof) = up.update_oneof {
                                if let yellowstone_grpc_proto::prelude::subscribe_update::UpdateOneof::Account(acc_update) = update_oneof {
                                    if let Some(account_info) = acc_update.account {
                                        let data = account_info.data;
                                        if data.len() >= 24 {
                                            let virtual_token = u64::from_le_bytes(
                                                data[8..16].try_into().unwrap_or([0; 8])
                                            );
                                            let virtual_sol = u64::from_le_bytes(
                                                data[16..24].try_into().unwrap_or([0; 8])
                                            );
                                            
                                            if virtual_token > 0 && virtual_sol > 0 {
                                                let price = virtual_sol as f64 / virtual_token as f64;
                                                let pos = {
                                                    let s = state.read().await;
                                                    if idx >= s.positions.len() {
                                                        break;
                                                    }
                                                    s.positions[idx].clone()
                                                };
                                                
                                                let current_val = (pos.held_tokens as f64 * price) / 1e9;
                                                let profit_pct = ((current_val / pos.buy_sol) - 1.0) * 100.0;
                                                let mcap = (1_000_000_000.0 * price) / 1e9;
                                                info!("Position {} profit: {:.2}% (MCAP: {:.0} SOL)", pos.mint, profit_pct, mcap);
                                                
                                                if profit_pct >= config.profit_threshold || profit_pct <= config.loss_threshold {
                                                    let min_sol_out = pos.buy_sol * (1.0 + config.loss_threshold / 100.0);
                                                    let wallet = {
                                                        let s = state.read().await;
                                                        s.wallet_keypair.as_ref()
                                                    };
                                                    if let Some(wallet) = wallet {
                                                        if let Err(e) = sell_token(&pos.mint, pos.held_tokens, min_sol_out, wallet, config.priority_fee, config.compute_units).await {
                                                            error!("Sell failed: {}", e);
                                                        } else {
                                                            let mut s = state.write().await;
                                                            if idx < s.positions.len() {
                                                                s.positions.remove(idx);
                                                            }
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            error!("GRPC stream error: {}", e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Failed to subscribe to account via GRPC: {}, falling back to RPC", e);
            }
        }
    }
    
    // Fallback RPC poll
    let rpc = RpcClient::new(RPC_URL.to_string());
    let curve = {
        let s = state.read().await;
        if idx >= s.positions.len() {
            return;
        }
        s.positions[idx].bonding_curve
    };
    
    loop {
        // Проверяем что позиция еще существует
        {
            let s = state.read().await;
            if idx >= s.positions.len() {
                break;
            }
        }
        
        match rpc.get_account_data(&curve).await {
            Ok(data) => {
                if data.len() >= 24 {
                    let virtual_token = u64::from_le_bytes(data[8..16].try_into().unwrap_or([0; 8]));
                    let virtual_sol = u64::from_le_bytes(data[16..24].try_into().unwrap_or([0; 8]));
                    
                    if virtual_token > 0 && virtual_sol > 0 {
                        let price = virtual_sol as f64 / virtual_token as f64;
                        let pos = {
                            let s = state.read().await;
                            if idx >= s.positions.len() {
                                break;
                            }
                            s.positions[idx].clone()
                        };
                        
                        let current_val = (pos.held_tokens as f64 * price) / 1e9;
                        let profit_pct = ((current_val / pos.buy_sol) - 1.0) * 100.0;
                        let mcap = (1_000_000_000.0 * price) / 1e9;
                        info!("Position {} profit: {:.2}% (MCAP: {:.0} SOL)", pos.mint, profit_pct, mcap);
                        
                        if profit_pct >= config.profit_threshold || profit_pct <= config.loss_threshold {
                            let min_sol_out = pos.buy_sol * (1.0 + config.loss_threshold / 100.0);
                            let wallet = {
                                let s = state.read().await;
                                s.wallet_keypair.as_ref()
                            };
                            if let Some(wallet) = wallet {
                                if let Err(e) = sell_token(&pos.mint, pos.held_tokens, min_sol_out, wallet, config.priority_fee, config.compute_units).await {
                                    error!("Sell failed: {}", e);
                                } else {
                                    let mut s = state.write().await;
                                    if idx < s.positions.len() {
                                        s.positions.remove(idx);
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                debug!("Failed to read bonding curve: {}", e);
            }
        }
        
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

// Мониторинг позиций и автопродажа
async fn monitor_positions(state: AppState) {
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await; // Проверяем каждые 5 секунд
        
        let (positions, config, wallet_pubkey) = {
            let sniper = state.read().await;
            if !sniper.running || sniper.positions.is_empty() {
                continue;
            }
            let config = sniper.config.clone();
            let wallet_pubkey = sniper.wallet_keypair.as_ref().map(|k| k.pubkey());
            (sniper.positions.clone(), config, wallet_pubkey)
        };
        
        if positions.is_empty() || config.is_none() || wallet_pubkey.is_none() {
            continue;
        }
        
        let config = config.unwrap();
        if !config.auto_sell_enabled {
            continue;
        }
        
        let wallet_pubkey = wallet_pubkey.unwrap();
        let rpc_client = RpcClient::new(RPC_URL.to_string());
        
        for position in positions {
            // Читаем баланс токенов из ATA
            let mint_pubkey = match Pubkey::from_str(&position.mint) {
                Ok(p) => p,
                Err(_) => continue,
            };
            
            let token_program = Pubkey::from_str(TOKEN_2022_PROGRAM_ID).unwrap();
            let associated_user = get_associated_token_address_with_program_id(
                &wallet_pubkey,
                &mint_pubkey,
                &token_program,
            );
            
            // Читаем баланс токенов
            let token_balance = match rpc_client.get_token_account_balance(&associated_user).await {
                Ok(balance) => balance.amount.parse::<u64>().unwrap_or(0),
                Err(_) => {
                    debug!("Token account not found or error reading balance for {}", position.mint);
                    continue;
                }
            };
            
            // Обновляем количество токенов в позиции
            {
                let mut sniper = state.write().await;
                if let Some(pos) = sniper.positions.iter_mut().find(|p| p.mint == position.mint) {
                    pos.held_tokens = token_balance;
                }
            }
            
            if token_balance == 0 {
                // Позиция закрыта, удаляем из списка
                let mut sniper = state.write().await;
                sniper.positions.retain(|p| p.mint != position.mint);
                continue;
            }
            
            // Читаем bonding curve для расчета текущей цены
            let bonding_curve_data = match rpc_client.get_account_data(&position.bonding_curve).await {
                Ok(data) => data,
                Err(_) => continue,
            };
            
            if bonding_curve_data.len() < 24 {
                continue;
            }
            
            let virtual_token_reserves = u64::from_le_bytes(
                bonding_curve_data[8..16].try_into().unwrap_or([0; 8])
            );
            let virtual_sol_reserves = u64::from_le_bytes(
                bonding_curve_data[16..24].try_into().unwrap_or([0; 8])
            );
            
            if virtual_token_reserves == 0 || virtual_sol_reserves == 0 {
                continue;
            }
            
            // Рассчитываем текущую стоимость токенов в SOL
            // Формула: sol_out = (tokens_in * virtual_sol_reserves) / (virtual_token_reserves + tokens_in)
            let tokens_in = token_balance as u128;
            let sol_out = (tokens_in
                .checked_mul(virtual_sol_reserves as u128)
                .and_then(|x| {
                    let denominator = (virtual_token_reserves as u128).checked_add(tokens_in)?;
                    x.checked_div(denominator)
                })
                .unwrap_or(0)) as u64;
            
            let sol_out_f64 = sol_out as f64 / 1e9;
            let buy_sol = position.buy_sol;
            let profit_pct = ((sol_out_f64 - buy_sol) / buy_sol) * 100.0;
            
            debug!("Position {}: {} tokens, value: {:.9} SOL, profit: {:.2}%", 
                   position.mint, token_balance, sol_out_f64, profit_pct);
            
            // Проверяем пороги для продажи
            if profit_pct >= config.profit_threshold || profit_pct <= config.loss_threshold {
                let min_sol_out = position.buy_sol * (1.0 + config.loss_threshold / 100.0);
                let wallet_keypair = {
                    let s = state.read().await;
                    s.wallet_keypair.as_ref().map(|k| (k.pubkey(), k))
                };
                if let Some((_pubkey, wallet)) = wallet_keypair {
                    if profit_pct >= config.profit_threshold {
                        info!("💰 Profit target reached! Selling {} tokens (profit: {:.2}%)", 
                              position.mint, profit_pct);
                    } else {
                        info!("🛑 Stop loss triggered! Selling {} tokens (loss: {:.2}%)", 
                              position.mint, profit_pct);
                    }
                    if let Err(e) = sell_token(&position.mint, token_balance, min_sol_out, &wallet, config.priority_fee, config.compute_units).await {
                        error!("Sell failed: {}", e);
                    } else {
                        let mut s = state.write().await;
                        s.positions.retain(|p| p.mint != position.mint);
                    }
                }
            }
        }
    }
}

// Основной цикл снайпера - использует TCP socket для МАКСИМАЛЬНОЙ СКОРОСТИ
async fn run_sniper_loop(state: AppState) {
    info!("🎯 Dev Wallet Sniper started (MAX SPEED MODE - TCP socket)");
    info!("🔌 Connecting to GRPC Proxy (TCP): {}:{}", GRPC_PROXY_TCP_HOST, GRPC_PROXY_TCP_PORT);
    
    // Основной цикл с переподключением
    loop {
        let running = {
            let sniper = state.read().await;
            sniper.running
        };
        
        if !running {
            info!("🛑 Sniper stopped");
            break;
        }
        
        // Подключаемся к TCP socket
        match tokio::net::TcpStream::connect((GRPC_PROXY_TCP_HOST, GRPC_PROXY_TCP_PORT)).await {
            Ok(mut stream) => {
                info!("✅ Connected to GRPC Proxy (TCP)");
                info!("⚡ MAX SPEED: Direct TCP socket (no HTTP overhead)");
                info!("📡 Listening for Create transactions...");
                
                let mut message_count = 0u64;
                
                loop {
                    let running = {
                        let sniper = state.read().await;
                        sniper.running
                    };
                    
                    if !running {
                        break;
                    }
                    
                    // Читаем длину данных (4 байта)
                    let mut len_bytes = [0u8; 4];
                    match stream.read_exact(&mut len_bytes).await {
                        Ok(_) => {
                            let len = u32::from_le_bytes(len_bytes) as usize;
                            
                            // Читаем данные
                            let mut data = vec![0u8; len];
                            match stream.read_exact(&mut data).await {
                                Ok(_) => {
                                    // Десериализуем Create транзакцию из бинарного формата
                                    match bincode::deserialize::<CreateTransaction>(&data) {
                                        Ok(create_tx) => {
                                            message_count += 1;
                                            
                                            // Показываем только первые 10 Create транзакций для отладки
                                            static CREATE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                                            let count = CREATE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                            
                                            if count < 10 {
                                                info!("✅ Received Create transaction #{} from TCP:", count + 1);
                                                info!("   Signature: {}", create_tx.signature);
                                                info!("   Mint: {}", create_tx.mint_address);
                                                info!("   Creator: {}", create_tx.creator_address);
                                            } else if count == 10 {
                                                info!("📊 Received 10+ Create transactions, reducing logging...");
                                            }
                                            
                                            if message_count % 10 == 0 {
                                                info!("📥 TCP messages received: {}", message_count);
                                            }
                                            
                                            // Обрабатываем транзакцию СРАЗУ (максимальная скорость!)
                                            let state_clone = state.clone();
                                            tokio::spawn(async move {
                                                if let Err(e) = handle_create_transaction(create_tx, state_clone).await {
                                                    error!("❌ Error handling create transaction: {}", e);
                                                }
                                            });
                                        }
                                        Err(e) => {
                                            error!("❌ Failed to deserialize CreateTransaction: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("❌ Failed to read data from TCP: {}", e);
                                    break;
                                }
                            }
                        }
                        Err(e) => {
                            error!("❌ Failed to read length from TCP: {}", e);
                            break;
                        }
                    }
                }
                
                warn!("🔄 TCP connection ended, reconnecting in 1 second...");
            }
            Err(e) => {
                error!("❌ Failed to connect to GRPC Proxy (TCP): {}", e);
                warn!("🔄 Attempting to reconnect in 1 second...");
            }
        }
        
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Инициализация логирования
    // Устанавливаем уровень логирования по умолчанию если не задан через env
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .with_ansi(true)
        .init();
    
    info!("🚀 Rust Sniper starting...");
    info!("📋 Logging initialized");
    
    info!("🚀 Rust Sniper starting...");
    
    // Инициализируем состояние
    let state = Arc::new(RwLock::new(SniperState {
        running: false,
        config: None,
        tracked_devs: HashSet::new(),
        stats: SniperStats::default(),
        wallet_keypair: None,
        token_purchased: false, // Флаг: был ли уже куплен токен в этой сессии
        positions: Vec::new(),
        grpc_client: None,
        cached_fee_recipient: None,
        cached_event_authority: None,
        cached_fee_config: None,
        cached_blockhash: None,
        cached_blockhash_slot: None,
    }));
    
    // Запускаем задачу обновления списка девов
    let refresh_state = state.clone();
    tokio::spawn(async move {
        refresh_tracked_devs(refresh_state).await;
    });
    
    // Запускаем задачу мониторинга позиций
    let monitor_state = state.clone();
    tokio::spawn(async move {
        monitor_positions(monitor_state).await;
    });
    
    // Создаем роутер
    let app = Router::new()
        .route("/api/sniper/config", post(update_config))
        .route("/api/sniper/start", post(start_sniper))
        .route("/api/sniper/stop", post(stop_sniper))
        .route("/api/sniper/status", get(get_status))
        .route("/api/sniper/stats", get(get_stats))
        .route("/health", get(|| async { "OK" }))
        .with_state(state);
    
    // Запускаем сервер
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8723")
        .await
        .context("Failed to bind to port 8723")?;
    
    info!("🚀 Rust Sniper API server started on port 8723");
    info!("📡 Waiting for configuration from UI...");
    
    axum::serve(listener, app).await?;
    
    Ok(())
}
