// GRPC клиент для подключения к Yellowstone Geyser
// Использует yellowstone-grpc-client вместо tonic

use yellowstone_grpc_client::{GeyserGrpcClient, ClientTlsConfig};
use yellowstone_grpc_proto::prelude::*;
use bs58;
use tracing::{info, error};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::error::Error;
use anyhow::{Result, Context};
use serde::{Serialize, Deserialize};
use std::sync::Arc;

// PUMP_FUN_PROGRAM_ID используется только в неиспользуемых функциях GRPC клиента
#[allow(dead_code)]
const PUMP_FUN_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTransaction {
    pub signature: String,
    pub mint_address: String,
    pub creator_address: String,
    pub slot: u64,
    pub is_create_v2: bool,
}

// Используем тип без явного CryptoProvider (используется default из main)
#[derive(Clone)]
pub struct GrpcClient {
    endpoint: String,
    api_token: String,
    client: Option<Arc<GeyserGrpcClient>>,
    subscribe_tx: Option<futures::channel::mpsc::Sender<SubscribeRequest>>,
}

impl GrpcClient {
    pub fn new(endpoint: String, api_token: String) -> Self {
        Self {
            endpoint,
            api_token,
            client: None,
            subscribe_tx: None,
        }
    }

    pub async fn connect(&mut self) -> Result<()> {
        info!("🔌 Connecting to GRPC endpoint: {}", self.endpoint);
        
        // Создаем клиент (как в rust_grpc_proxy)
        let client = GeyserGrpcClient::build_from_shared(self.endpoint.clone())
            .map_err(|e| {
                error!("Failed to create GRPC client builder: {}", e);
                e
            })?
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(|e| {
                error!("Failed to configure TLS: {}", e);
                e
            })?
            .connect()
            .await
            .map_err(|e| {
                error!("Failed to connect to GRPC endpoint: {}", e);
                if let Some(source) = Error::source(&e) {
                    error!("Source error: {}", source);
                }
                e
            })?;
        
        self.client = Some(Arc::new(client));
        info!("✅ Connected to GRPC endpoint");
        
        Ok(())
    }

    pub async fn subscribe_to_creates(&mut self) -> Result<()> {
        info!("📡 Subscribing to Create transactions...");
        
        let client = self.client.as_ref()
            .context("Client not connected. Call connect() first")?;
        
        // Получаем stream и subscribe_tx
        let (subscribe_tx, _updates_stream): (
            futures::channel::mpsc::Sender<SubscribeRequest>,
            _
        ) = client.subscribe().await?;
        self.subscribe_tx = Some(subscribe_tx.clone());
        
        // Создаем filter для транзакций Pump.fun
        let mut transactions_filter = HashMap::new();
        transactions_filter.insert(
            "pump_fun".to_string(),
            SubscribeRequestFilterTransactions {
            account_include: vec![PUMP_FUN_PROGRAM_ID.to_string()],
                vote: Some(false),
                failed: Some(false),
                signature: None,
            account_exclude: vec![],
            account_required: vec![],
            },
        );
        
        // Создаем subscription request
        let request = SubscribeRequest {
            transactions: transactions_filter,
            commitment: Some(CommitmentLevel::Processed as i32),
            ..Default::default()
        };
        
        // Отправляем subscription request
        subscribe_tx.send(request).await?;
        info!("✅ Subscribed to Create transactions");
        
        Ok(())
    }
    
    pub async fn subscribe_to_account(&mut self, pubkey: &str) -> Result<impl StreamExt<Item = Result<SubscribeUpdate, yellowstone_grpc_client::GeyserGrpcClientError>>> {
        let client = self.client.as_ref()
            .context("Client not connected. Call connect() first")?;
        
        let (mut subscribe_tx, updates_stream) = client.subscribe().await?;
        let mut accounts_filter = HashMap::new();
        accounts_filter.insert(
            "bonding_curve".to_string(),
            SubscribeRequestFilterAccounts {
                account: vec![pubkey.to_string()],
                owner: vec![],
                filters: vec![],
            },
        );
        let request = SubscribeRequest {
            accounts: accounts_filter,
            commitment: Some(CommitmentLevel::Processed as i32),
            ..Default::default()
        };
        subscribe_tx.send(request).await?;
        Ok(updates_stream)
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        info!("🔌 Disconnecting from GRPC");
        self.subscribe_tx = None;
        self.client = None;
        Ok(())
    }
}

// Парсинг Create транзакции из GRPC данных
// Адаптировано из parseCreateTransaction в filter_grpc_create.js
#[allow(dead_code)]
pub fn parse_create_transaction_from_grpc(tx: &SubscribeUpdateTransaction) -> Option<CreateTransaction> {
    let pump_fun_program_id = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
    
    // Проверяем метаданные транзакции
    let tx_data = tx.transaction.as_ref()?;
    let meta = tx_data.meta.as_ref()?;
    
    // Проверяем логи на наличие Pump.fun и Create
    let log_str = meta.log_messages.join("\n");
    
    let has_pump_fun = log_str.contains(pump_fun_program_id);
    let is_create = log_str.contains("Instruction: Create") && !log_str.contains("CreateV2");
    let is_create_v2 = log_str.contains("Instruction: CreateV2");
    
    if !has_pump_fun || (!is_create && !is_create_v2) {
        return None;
    }

    // Получаем подпись
    let signature = if !tx_data.signature.is_empty() {
        bs58::encode(&tx_data.signature).into_string()
    } else if let Some(tx_transaction) = tx_data.transaction.as_ref() {
        if let Some(first_sig) = tx_transaction.signatures.first() {
            bs58::encode(first_sig).into_string()
        } else {
            return None;
        }
    } else {
        return None;
    };
    
    // Получаем creator (первый аккаунт)
    let creator_address = if let Some(tx_transaction) = tx_data.transaction.as_ref() {
        if let Some(message) = tx_transaction.message.as_ref() {
            if let Some(first_key) = message.account_keys.first() {
                bs58::encode(first_key).into_string()
            } else {
        return None;
    }
        } else {
            return None;
        }
    } else {
        return None;
    };
    
    // Получаем mint из post_token_balances
    let post_balances = &meta.post_token_balances;
    let pre_balances = &meta.pre_token_balances;
    
    let pre_mints: std::collections::HashSet<String> = pre_balances.iter()
        .map(|b| b.mint.clone())
        .collect();
    
    let mut candidate_mints = vec![];
    for balance in post_balances {
        let mint = &balance.mint;
        if !pre_mints.contains(mint) && !mint.contains("11111111111111111111111111111111") {
                candidate_mints.push(mint.clone());
            }
        }
    
    let mint_address = candidate_mints.iter()
        .find(|m| m.ends_with("pump"))
        .or_else(|| candidate_mints.first())
        .cloned()?;
    
    // Получаем slot из верхнего уровня SubscribeUpdateTransaction
    let slot = tx.slot as u64;
    
    Some(CreateTransaction {
        signature,
        mint_address,
        creator_address,
        slot,
        is_create_v2,
    })
}
