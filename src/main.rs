mod instruction_account_mapper;
mod serialization;
mod token_serializable;

#[allow(unused_imports)]
use {
    clap::Parser as ClapParser,
    futures::{sink::SinkExt, stream::StreamExt},
    instruction_account_mapper::{AccountMetadata, Idl, InstructionAccountMapper},
    log::{error, info, debug, warn},
    serde::Deserialize,
    serde::{Serialize},
    serde_json::Value,
    std::{collections::HashMap, env, fs, path::PathBuf, str::FromStr, sync::Arc, time::{Duration, SystemTime, UNIX_EPOCH}, io::Write},
    tokio::time::interval,
    tonic::transport::channel::ClientTlsConfig,
    yellowstone_grpc_client::{GeyserGrpcClient, Interceptor},
    yellowstone_grpc_proto::{
        geyser::SubscribeRequestFilterTransactions,
        geyser::SubscribeRequestFilterAccounts,
        prelude::{
            subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestPing,
        },
    },
    pump_interface::instructions::PumpProgramIx,
    pump_interface::accounts::{BondingCurve, BondingCurveAccount, Global, GlobalAccount, BONDING_CURVE_ACCOUNT_DISCM, GLOBAL_ACCOUNT_DISCM},
    solana_sdk::{pubkey::Pubkey, instruction::AccountMeta},
    chrono::{TimeZone, Utc, FixedOffset, DateTime},
    spl_token::instruction::TokenInstruction,
    token_serializable::convert_to_serializable,
    dashmap::DashMap,
    serde_json::json,
    redis::AsyncCommands,
};

type TxnFilterMap = HashMap<String, SubscribeRequestFilterTransactions>;
type AccountFilterMap = HashMap<String, SubscribeRequestFilterAccounts>;

// 定义常量
const PUMP_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const CACHE_CLEANUP_INTERVAL_SECS: u64 = 10; // 缓存清理间隔（秒）
const MAX_CACHE_AGE_SECS: u64 = 30; // 内存缓存最大有效期（秒）
const REDIS_CACHE_AGE_SECS: u64 = 3600; // Redis缓存最大有效期（1小时）

// 定义缓存项结构
#[derive(Debug, Clone)]
struct CacheItem {
    data: String,
    timestamp: SystemTime,
}

// 定义缓存结构
struct TransactionCache {
    // 交易缓存
    buy_transactions: DashMap<String, CacheItem>,
    sell_transactions: DashMap<String, CacheItem>,
    // 账户缓存
    account_data: DashMap<String, CacheItem>,
    // 最新的账户数据，用于关联到交易中
    latest_account_data: DashMap<String, String>, // mint -> account_data
    // 账户中最新的虚拟储备信息，用于与交易对比
    latest_reserves: DashMap<String, (u64, u64)>, // mint -> (virtual_token_reserves, virtual_sol_reserves)
    redis_client: Arc<redis::Client>,
}

impl TransactionCache {
    fn new(redis_client: Arc<redis::Client>) -> Self {
        Self {
            buy_transactions: DashMap::new(),
            sell_transactions: DashMap::new(),
            account_data: DashMap::new(),
            latest_account_data: DashMap::new(),
            latest_reserves: DashMap::new(),
            redis_client,
        }
    }

    // 缓存买入交易
    fn cache_buy_transaction(&self, signature: &str, data: String, mint: Option<&str>) {
        // 首先记录函数调用信息
        info!("[缓存] 缓存买入交易 - 签名: {}, Mint: {:?}", signature, mint);
        
        let mut enhanced_data = data.clone();
        
        // 如果提供了mint参数，尝试获取并添加关联的账户数据
        if let Some(mint_address) = mint {
            // 添加Mint信息
            enhanced_data.push_str("\n\nMINT地址:\n");
            enhanced_data.push_str(mint_address);
            
            // 计算并添加绑定曲线账户信息
            if let Some(curve_account) = calculate_curve_account_from_mint(mint_address) {
                info!("[关联] Buy交易({})关联到曲线账户({})", signature, curve_account);
                enhanced_data.push_str("\n\n关联曲线账户:\n");
                enhanced_data.push_str(&curve_account);
                
                // 获取曲线账户数据
                if let Some(curve_data) = self.get_account_data(&curve_account) {
                    enhanced_data.push_str("\n\n绑定曲线账户数据:\n");
                    enhanced_data.push_str(&curve_data);
                    
                    // 提取并添加虚拟储备信息
                    if let Some((vt, vs)) = extract_reserves_from_account_data(&curve_data) {
                        info!("[储备] Buy交易({})的虚拟储备 - 代币: {}, SOL: {}", signature, vt, vs);
                        enhanced_data.push_str(&format!("\n\n虚拟储备信息:\n虚拟代币储备: {}\n虚拟SOL储备: {}", vt, vs));
                        
                        // 计算并添加价格信息
                        let price = calculate_price(vt, vs);
                        info!("[价格] Buy交易({})的代币价格: {} SOL", signature, price);
                        enhanced_data.push_str(&format!("\n\n价格信息:\n当前价格: {} SOL", price));
                    } else {
                        warn!("[储备] 无法从曲线账户({})提取虚拟储备信息", curve_account);
                    }
                } else {
                    warn!("[缓存] 未找到曲线账户({})的数据", curve_account);
                }
            } else {
                warn!("[关联] 无法为Mint({})计算曲线账户", mint_address);
            }
        }
        
        let cache_item = CacheItem {
            data: enhanced_data.clone(),
            timestamp: SystemTime::now(),
        };
        self.buy_transactions.insert(signature.to_string(), cache_item);

        let client_clone = Arc::clone(&self.redis_client);
        let key = signature.to_string();
        tokio::spawn(async move {
            let mut con = match client_clone.get_multiplexed_tokio_connection().await {
                Ok(c) => c,
                Err(e) => {
                    error!("[Redis] 获取连接失败 (buy_tx - sig: {}): {}", key, e);
                    return;
                }
            };
            if let Err(e) = con.set::<_, _, ()>(&key, &enhanced_data).await {
                error!("[Redis] 缓存买入交易失败 (sig: {}): {}", key, e);
            } else {
                debug!("[Redis] 成功缓存买入交易 (sig: {})", key);
                if let Err(e) = con.expire::<_, ()>(&key, REDIS_CACHE_AGE_SECS as i64).await {
                    error!("[Redis] 设置买入交易过期时间失败 (sig: {}): {}", key, e);
                }
            }
        });
    }

    // 缓存卖出交易
    fn cache_sell_transaction(&self, signature: &str, data: String, mint: Option<&str>) {
        // 首先记录函数调用信息
        info!("[缓存] 缓存卖出交易 - 签名: {}, Mint: {:?}", signature, mint);
        
        let mut enhanced_data = data.clone();
        
        // 如果提供了mint参数，尝试获取并添加关联的账户数据
        if let Some(mint_address) = mint {
            // 添加Mint信息
            enhanced_data.push_str("\n\nMINT地址:\n");
            enhanced_data.push_str(mint_address);
            
            // 计算并添加绑定曲线账户信息
            if let Some(curve_account) = calculate_curve_account_from_mint(mint_address) {
                info!("[关联] Sell交易({})关联到曲线账户({})", signature, curve_account);
                enhanced_data.push_str("\n\n关联曲线账户:\n");
                enhanced_data.push_str(&curve_account);
                
                // 获取曲线账户数据
                if let Some(curve_data) = self.get_account_data(&curve_account) {
                    enhanced_data.push_str("\n\n绑定曲线账户数据:\n");
                    enhanced_data.push_str(&curve_data);
                    
                    // 提取并添加虚拟储备信息
                    if let Some((vt, vs)) = extract_reserves_from_account_data(&curve_data) {
                        info!("[储备] Sell交易({})的虚拟储备 - 代币: {}, SOL: {}", signature, vt, vs);
                        enhanced_data.push_str(&format!("\n\n虚拟储备信息:\n虚拟代币储备: {}\n虚拟SOL储备: {}", vt, vs));
                        
                        // 计算并添加价格信息
                        let price = calculate_price(vt, vs);
                        info!("[价格] Sell交易({})的代币价格: {} SOL", signature, price);
                        enhanced_data.push_str(&format!("\n\n价格信息:\n当前价格: {} SOL", price));
                    } else {
                        warn!("[储备] 无法从曲线账户({})提取虚拟储备信息", curve_account);
                    }
                } else {
                    warn!("[缓存] 未找到曲线账户({})的数据", curve_account);
                }
            } else {
                warn!("[关联] 无法为Mint({})计算曲线账户", mint_address);
            }
        }
        
        let cache_item = CacheItem {
            data: enhanced_data.clone(),
            timestamp: SystemTime::now(),
        };
        self.sell_transactions.insert(signature.to_string(), cache_item);

        let client_clone = Arc::clone(&self.redis_client);
        let key = signature.to_string();
        tokio::spawn(async move {
            let mut con = match client_clone.get_multiplexed_tokio_connection().await {
                Ok(c) => c,
                Err(e) => {
                    error!("[Redis] 获取连接失败 (sell_tx - sig: {}): {}", key, e);
                    return;
                }
            };
            if let Err(e) = con.set::<_, _, ()>(&key, &enhanced_data).await {
                error!("[Redis] 缓存卖出交易失败 (sig: {}): {}", key, e);
            } else {
                debug!("[Redis] 成功缓存卖出交易 (sig: {})", key);
                if let Err(e) = con.expire::<_, ()>(&key, REDIS_CACHE_AGE_SECS as i64).await {
                    error!("[Redis] 设置卖出交易过期时间失败 (sig: {}): {}", key, e);
                }
            }
        });
    }

    // 缓存账户数据
    fn cache_account_data(&self, pubkey: &str, data: String) {
        let cache_item = CacheItem {
            data: data.clone(),
            timestamp: SystemTime::now(),
        };
        self.account_data.insert(pubkey.to_string(), cache_item);

        // 尝试提取mint地址
        if let Some(mint) = extract_mint_address_from_account_data(&data) {
            debug!("[关联] 从账户数据中提取到mint地址: {}, 账户: {}", mint, pubkey);
            self.latest_account_data.insert(mint.clone(), data.clone());
            
            // 尝试提取虚拟储备信息
            if let Some((virtual_token_reserves, virtual_sol_reserves)) = extract_reserves_from_account_data(&data) {
                debug!("[储备] 提取到虚拟储备 - Mint: {}, VT: {}, VS: {}", 
                    mint, virtual_token_reserves, virtual_sol_reserves);
                self.latest_reserves.insert(mint, (virtual_token_reserves, virtual_sol_reserves));
            }
        }

        let client_clone = Arc::clone(&self.redis_client);
        let key = pubkey.to_string();
        tokio::spawn(async move {
            let mut con = match client_clone.get_multiplexed_tokio_connection().await {
                Ok(c) => c,
                Err(e) => {
                    error!("[Redis] 获取连接失败 (account - key: {}): {}", key, e);
                    return;
                }
            };
            if let Err(e) = con.set::<_, _, ()>(&key, &data).await {
                error!("[Redis] 缓存账户数据失败 (key: {}): {}", key, e);
            } else {
                debug!("[Redis] 成功缓存账户数据 (key: {})", key);
                if let Err(e) = con.expire::<_, ()>(&key, REDIS_CACHE_AGE_SECS as i64).await {
                    error!("[Redis] 设置账户数据过期时间失败 (key: {}): {}", key, e);
                }
            }
        });
    }

    // 获取最新的账户数据（按mint地址）
    fn get_latest_account_data(&self, mint: &str) -> Option<String> {
        self.latest_account_data.get(mint).map(|data| data.clone())
    }
    
    // 获取最新的虚拟储备数据（按mint地址）
    fn get_latest_reserves(&self, mint: &str) -> Option<(u64, u64)> {
        self.latest_reserves.get(mint).map(|reserves| *reserves)
    }

    // 获取买入交易
    fn get_buy_transaction(&self, signature: &str) -> Option<String> {
        self.buy_transactions.get(signature).map(|item| item.data.clone())
    }

    // 获取卖出交易
    fn get_sell_transaction(&self, signature: &str) -> Option<String> {
        self.sell_transactions.get(signature).map(|item| item.data.clone())
    }

    // 获取账户数据
    fn get_account_data(&self, pubkey: &str) -> Option<String> {
        self.account_data.get(pubkey).map(|item| item.data.clone())
    }

    // 清理过期缓存
    fn cleanup(&self, max_age: Duration) {
        let now = SystemTime::now();
        let mut buy_removed = 0;
        let mut sell_removed = 0;
        let mut account_removed = 0;

        // 清理买入交易缓存
        self.buy_transactions.retain(|_, item| {
            match now.duration_since(item.timestamp) {
                Ok(age) if age > max_age => {
                    buy_removed += 1;
                    false
                },
                _ => true,
            }
        });

        // 清理卖出交易缓存
        self.sell_transactions.retain(|_, item| {
            match now.duration_since(item.timestamp) {
                Ok(age) if age > max_age => {
                    sell_removed += 1;
                    false
                },
                _ => true,
            }
        });

        // 清理账户数据缓存
        self.account_data.retain(|_, item| {
            match now.duration_since(item.timestamp) {
                Ok(age) if age > max_age => {
                    account_removed += 1;
                    false
                },
                _ => true,
            }
        });

        if buy_removed > 0 || sell_removed > 0 || account_removed > 0 {
            debug!("缓存清理: 移除 {} 个买入交易, {} 个卖出交易, {} 个账户数据", 
                buy_removed, sell_removed, account_removed);
        }
    }

    // 获取缓存统计信息
    fn get_stats(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.buy_transactions.len(),
            self.sell_transactions.len(),
            self.account_data.len(),
            self.latest_account_data.len(),
            self.latest_reserves.len(),
        )
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Features {
    basic_transaction_monitoring: bool,
    advanced_event_detection: bool,
    token_transaction_monitoring: bool,
    account_monitoring: bool,
    log_to_file: bool,
    log_file_path: String,
    enable_cache: bool,
}

#[derive(Debug, Deserialize)]
struct Config {
    grpc_endpoint: String,
    monitored_addresses: Vec<String>,
    pump_program_id: Option<String>,
    pump_idl_path: Option<String>,
    token_idl_path: Option<String>,
    features: Option<Features>,
    redis_url: String,
}

impl Config {
    fn load(path: PathBuf) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    fn load_pump_idl(&self) -> anyhow::Result<Option<Idl>> {
        if let Some(idl_path) = &self.pump_idl_path {
            let content = fs::read_to_string(idl_path)?;
            Ok(Some(serde_json::from_str(&content)?))
        } else {
            Ok(None)
        }
    }
    
    fn load_token_idl(&self) -> anyhow::Result<Option<Idl>> {
        if let Some(idl_path) = &self.token_idl_path {
            let content = fs::read_to_string(idl_path)?;
            Ok(Some(serde_json::from_str(&content)?))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone, ClapParser)]
#[clap(author, version, about = "Solana 交易监控工具")]
struct Args {
    #[clap(short, long, help = "配置文件路径", default_value = "config.toml")]
    config: PathBuf,
}

impl Args {
    async fn connect(&self, endpoint: String) -> anyhow::Result<GeyserGrpcClient<impl Interceptor>> {
        GeyserGrpcClient::build_from_shared(endpoint)?
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(10))
            .tls_config(ClientTlsConfig::new().with_native_roots())?
            .max_decoding_message_size(1024 * 1024 * 1024)
            .connect()
            .await
            .map_err(Into::into)
    }

    fn get_txn_updates(&self, addresses: Vec<String>, program_id: &str) -> anyhow::Result<SubscribeRequest> {
        let mut transactions: TxnFilterMap = HashMap::new();
        
        // 构建监听地址列表，包含用户地址和程序ID
        let mut all_accounts = addresses.clone();
        all_accounts.push(program_id.to_string());

        transactions.insert(
            "client".to_owned(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                account_include: all_accounts,
                account_exclude: vec![],
                account_required: vec![],
                signature: None,
            },
        );

        Ok(SubscribeRequest {
            accounts: HashMap::default(),
            slots: HashMap::default(),
            transactions,
            transactions_status: HashMap::default(),
            blocks: HashMap::default(),
            blocks_meta: HashMap::default(),
            entry: HashMap::default(),
            commitment: Some(CommitmentLevel::Processed as i32),
            accounts_data_slice: Vec::default(),
            ping: None,
            from_slot: None,
        })
    }
    
    fn get_account_updates(&self, program_id: &str) -> anyhow::Result<SubscribeRequest> {
        let mut accounts: AccountFilterMap = HashMap::new();
        
        accounts.insert(
            "accountData".to_owned(),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: vec![program_id.to_string()],
                nonempty_txn_signature: None,
                filters: vec![],
            },
        );
        
        Ok(SubscribeRequest {
            accounts,
            slots: HashMap::default(),
            transactions: HashMap::default(),
            transactions_status: HashMap::default(),
            blocks: HashMap::default(),
            blocks_meta: HashMap::default(),
            entry: HashMap::default(),
            commitment: Some(CommitmentLevel::Processed as i32),
            accounts_data_slice: Vec::default(),
            ping: None,
            from_slot: None,
        })
    }
}

/// Converts a string to camel case.
fn to_camel_case(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first_char) => first_char.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Extracts the instruction name and converts it to camel case.
fn get_instruction_name_with_typename(instruction: &TokenInstruction) -> String {
    let debug_string = format!("{:?}", instruction);
    if let Some(first_brace) = debug_string.find(" {") {
        let name = &debug_string[..first_brace]; // Extract name before `{`
        to_camel_case(name)
    } else {
        to_camel_case(&debug_string) // Directly convert unit variant names
    }
}

#[derive(Debug)]
pub enum DecodedAccount {
    BondingCurve(BondingCurve),
    Global(Global),
}

#[derive(Debug)]
pub struct AccountDecodeError {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct DecodedInstruction {
    pub name: String,
    pub accounts: Vec<AccountMetadata>,
    pub data: serde_json::Value,
    #[serde(serialize_with = "serialization::serialize_pubkey")]
    pub program_id: Pubkey,
    #[serde(serialize_with = "serialization::serialize_option_pubkey")]
    pub parent_program_id: Option<Pubkey>,
}

/// 使用虚拟储备数据计算价格
fn calculate_price(vt: u64, vs: u64) -> f64 {
    if vt == 0 {
        return 0.0; // 避免除以零
    }
    // 价格公式: vs/vt （SOL储备/代币储备）
    // 转换为SOL单位 (lamports -> SOL)
    (vs as f64) / (vt as f64) / 1_000_000_000.0
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();
    let config = Config::load(args.config.clone())?;
    let features = config.features.clone().unwrap_or_else(|| {
        warn!("配置文件中未找到 'features' 部分，将使用默认特性集。");
        Features {
            basic_transaction_monitoring: true,
            advanced_event_detection: true,
            token_transaction_monitoring: true,
            account_monitoring: true,
            log_to_file: false,
            log_file_path: "".to_string(),
            enable_cache: true,
        }
    });
    
    let redis_client = Arc::new(redis::Client::open(config.redis_url.as_str()).map_err(|e| {
        error!("[Redis] 连接 Redis 失败 ({}): {}", config.redis_url, e);
        anyhow::anyhow!("[Redis] 连接 Redis 失败: {}", e)
    })?);
    info!("[Redis] 已连接到: {}", config.redis_url);
    
    let pump_idl = config.load_pump_idl()?;
    let token_idl = config.load_token_idl()?;
    
    let program_id = config.pump_program_id.as_deref().unwrap_or(PUMP_PROGRAM_ID);
    
    // 输出配置信息
    info!("正在监听地址: {:?}", config.monitored_addresses);
    info!("PumpFun 程序 ID: {}", program_id);
    info!("功能配置:");
    info!("  - 基本交易监控: {}", features.basic_transaction_monitoring);
    info!("  - 高级事件检测: {}", features.advanced_event_detection);
    info!("  - Token交易监控: {}", features.token_transaction_monitoring);
    log::debug!("  - 账户监控: {}", features.account_monitoring);
    info!("  - 记录到文件: {}", features.log_to_file);
    info!("  - 启用缓存: {}", features.enable_cache);
    
    if pump_idl.is_some() {
        log::debug!("已加载 PumpFun IDL 文件");
    }
    
    if token_idl.is_some() {
        log::debug!("已加载 Token IDL 文件");
    }
    
    // 创建日志文件目录（如果启用了记录到文件）
    if features.log_to_file {
        let log_dir = std::path::Path::new(&features.log_file_path).parent()
            .expect("无法获取日志文件目录");
        if !log_dir.exists() {
            fs::create_dir_all(log_dir)?;
            info!("创建日志目录: {:?}", log_dir);
        }
    }
    
    // 创建缓存并启动清理任务
    let cache = if features.enable_cache {
        let cache = Arc::new(TransactionCache::new(Arc::clone(&redis_client)));
        let cache_clone = Arc::clone(&cache);
        
        // 启动缓存清理任务
        tokio::spawn(async move {
            let mut interval = interval(Duration::from_secs(CACHE_CLEANUP_INTERVAL_SECS));
            loop {
                interval.tick().await;
                cache_clone.cleanup(Duration::from_secs(MAX_CACHE_AGE_SECS));
                
                // 每10次清理（约100秒）输出一次统计信息
                let (buy_count, sell_count, account_count, latest_account_count, latest_reserves_count) = cache_clone.get_stats();
                debug!("缓存统计: {} 个买入交易, {} 个卖出交易, {} 个账户数据, {} 个最新账户数据, {} 个最新储备数据",
                    buy_count, sell_count, account_count, latest_account_count, latest_reserves_count);
            }
        });
        
        Some(cache)
    } else {
        None
    };
    
    let client_endpoint = config.grpc_endpoint.clone();
    info!("已连接到 gRPC 端点，开始监控...");

    // 两个监控模式同时启动，分别在不同的任务中运行
    if features.basic_transaction_monitoring {
        info!("启用交易监控模式");
        let client_txn = args.connect(client_endpoint.clone()).await?;
        let request_txn = args.get_txn_updates(config.monitored_addresses.clone(), program_id)?;
        let pump_idl_clone = pump_idl.clone();
        let token_idl_clone = token_idl.clone();
        let program_id_str = program_id.to_string();
        let features_clone = features.clone();
        let cache_clone = cache.clone();
        
        tokio::spawn(async move {
            if let Err(e) = geyser_subscribe(
                client_txn, 
                request_txn, 
                pump_idl_clone, 
                token_idl_clone, 
                &program_id_str, 
                &features_clone, 
                cache_clone
            ).await {
                error!("交易监控错误: {}", e);
            }
        });
    }
    
    if features.account_monitoring {
        log::debug!("启用账户监控模式");
        let client_acct = args.connect(client_endpoint).await?;
        let request_acct = args.get_account_updates(program_id)?;
        let features_clone = features.clone();
        let cache_clone = cache.clone();
        
        tokio::spawn(async move {
            if let Err(e) = geyser_subscribe_accounts(
                client_acct, 
                request_acct, 
                &features_clone, 
                cache_clone
            ).await {
                error!("账户监控错误: {}", e);
            }
        });
    }
    
    // 让主任务保持运行
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn geyser_subscribe(
    mut client: GeyserGrpcClient<impl Interceptor>,
    request: SubscribeRequest,
    _pump_idl: Option<Idl>,
    _token_idl: Option<Idl>,
    program_id: &str,
    features: &Features,
    cache: Option<Arc<TransactionCache>>,
) -> anyhow::Result<()> {
    // 在使用request前先提取监控地址
    let monitored_addresses: Vec<String> = if let Some(txn_filter) = request.transactions.get("client") {
        // 过滤掉程序ID本身，只保留用户要监听的地址
        txn_filter.account_include.iter()
            .filter(|addr| *addr != program_id)
            .cloned()
            .collect()
    } else {
        vec![]
    };
    
    // 精简日志输出
    log::debug!("过滤后监听的地址: {:?}", monitored_addresses);
    
    // 克隆 request 或使用可变引用
    let (mut subscribe_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;

    // 打开日志文件（如果启用）
    let mut log_file = if features.log_to_file {
        Some(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&features.log_file_path)?
        )
    } else {
        None
    };

    while let Some(message) = stream.next().await {
        match message {
            Ok(msg) => match msg.update_oneof {
                Some(UpdateOneof::Transaction(update)) => {
                    if let Some(txn) = update.transaction {
                        let signature = bs58::encode(&txn.signature).into_string();
                        
                        // 仅调试级别记录所有交易
                        log::debug!("收到新交易，签名: {}", signature);
                        
                        // 检查是否和监听的地址相关
                        let mut is_monitored_address_involved = false;
                        
                        // 如果有消息数据，检查账户
                        if let Some(raw_transaction) = &txn.transaction {
                            if let Some(raw_message) = &raw_transaction.message {
                                // 提取交易中涉及的所有地址
                                for account_key in &raw_message.account_keys {
                                    let account_str = bs58::encode(account_key).into_string();
                                    // 检查是否在监控地址列表中（排除程序ID本身）
                                    if monitored_addresses.contains(&account_str) && account_str != program_id {
                                        is_monitored_address_involved = true;
                                        break;
                                    }
                                }
                            }
                        }

                        // 只有当基本交易监控开启时才处理
                        if !features.basic_transaction_monitoring {
                            continue;
                        }

                        // 处理 PumpFun 交易
                        if let Some(raw_transaction) = txn.transaction {
                            if let Some(raw_message) = raw_transaction.message {
                                // 遍历所有指令，不使用索引变量
                                for instruction in raw_message.instructions.iter() {
                                    // 获取程序 ID
                                    let program_id_index = instruction.program_id_index as usize;
                                    if program_id_index < raw_message.account_keys.len() {
                                        let program_id_bytes = &raw_message.account_keys[program_id_index];
                                        
                                        // 检查是否是 PumpFun 程序
                                        if let Ok(program_pubkey) = Pubkey::from_str(program_id) {
                                            let program_bytes = program_pubkey.to_bytes().to_vec();
                                            if program_id_bytes == &program_bytes {
                                                // 尝试解析指令
                                                match PumpProgramIx::deserialize(&instruction.data) {
                                                    Ok(decoded_ix) => {
                                                        let timestamp_millis = SystemTime::now()
                                                            .duration_since(UNIX_EPOCH)
                                                            .expect("Time went backwards");
                                                        
                                                        // 创建UTC时间
                                                        let utc_datetime = Utc.timestamp_millis_opt(
                                                            timestamp_millis.as_millis() as i64
                                                        ).unwrap();
                                                        
                                                        // 转换为东八区（北京时间，UTC+8）
                                                        let beijing_offset = FixedOffset::east_opt(8 * 3600).unwrap(); // 8小时 = 8 * 3600秒
                                                        let beijing_time = utc_datetime.with_timezone(&beijing_offset);
                                                        
                                                        // 格式化为ISO 8601格式，显示+08:00时区信息
                                                        let formatted_time = beijing_time.format("%Y-%m-%dT%H:%M:%S%.3f+08:00").to_string();
                                                        
                                                        // 根据是否涉及监控地址以及功能开关选择分析方式
                                                        let _advanced_analysis = features.advanced_event_detection;
                                                        
                                                        // 使用官方高效处理方式，创建DecodedInstruction
                                                        if let Some(ref idl) = _pump_idl {
                                                            // 创建AccountMeta列表
                                                            let account_metas: Vec<AccountMeta> = instruction.accounts.iter()
                                                                .filter(|&&acc_idx| {
                                                                    // 确保索引在数组范围内
                                                                    (acc_idx as usize) < raw_message.account_keys.len()
                                                                })
                                                                .map(|&acc_idx| {
                                                                    let pubkey = Pubkey::new_from_array(
                                                                        raw_message.account_keys[acc_idx as usize]
                                                                            .clone()
                                                                            .try_into()
                                                                            .unwrap_or_default()
                                                                    );
                                                                    
                                                                    // 简化处理，仅判断是否为签名者
                                                                    let is_signer = raw_message.header.as_ref().map_or(false, |h| {
                                                                        (acc_idx as usize) < (h.num_required_signatures as usize)
                                                                    });
                                                                    
                                                                    // 简化可写判断
                                                                    let is_writable = true; // 默认可写，简化处理
                                                                    
                                                                    AccountMeta {
                                                                        pubkey,
                                                                        is_signer,
                                                                        is_writable,
                                                                    }
                                                                })
                                                                .collect();
                                                            
                                                            // 使用InstructionAccountMapper映射账户
                                                            if let Ok(mapped_accounts) = idl.map_accounts(&account_metas, &decoded_ix.name()) {
                                                                let decoded_instruction = DecodedInstruction {
                                                                    name: decoded_ix.name(),
                                                                    accounts: mapped_accounts,
                                                                    data: match decoded_ix {
                                                                        PumpProgramIx::Buy(ref buy_args) => {
                                                                            // 手动创建Buy指令的JSON对象
                                                                            json!({
                                                                                "buy": {
                                                                                    "amount": buy_args.amount,
                                                                                    "max_sol_cost": buy_args.max_sol_cost
                                                                                }
                                                                            })
                                                                        },
                                                                        PumpProgramIx::Sell(ref sell_args) => {
                                                                            // 手动创建Sell指令的JSON对象
                                                                            json!({
                                                                                "sell": {
                                                                                    "amount": sell_args.amount,
                                                                                    "min_sol_output": sell_args.min_sol_output
                                                                                }
                                                                            })
                                                                        },
                                                                        _ => {
                                                                            // 对于其他指令，只提供名称
                                                                            json!({ decoded_ix.name(): {} })
                                                                        }
                                                                    },
                                                                    program_id: Pubkey::from_str(program_id).unwrap(),
                                                                    parent_program_id: None,
                                                                };
                                                                
                                                                // 序列化为JSON以便提取mint信息
                                                                if let Ok(json_string) = serde_json::to_string_pretty(&decoded_instruction) {
                                                                    let parsed_json: Value = serde_json::from_str(&json_string).unwrap_or_default();
                                                                    
                                                                    // 从JSON中提取需要的信息
                                                                    let mut mint_address = "未知".to_string();
                                                                    if let Some(accounts) = parsed_json["accounts"].as_array() {
                                                                        if let Some(mint) = accounts.iter().find(|obj| obj["name"] == "mint") {
                                                                            mint_address = mint["pubkey"].as_str().unwrap_or("未知").to_string();
                                                                        }
                                                                    }
                                                                    
                                                                    // 从JSON中提取指令数据
                                                                    match decoded_ix {
                                                                        PumpProgramIx::Buy(ref buy_args) => {
                                                                            let log_message = format!(
                                                                                "TYPE: Buy\nMINT: {}\nTOKEN AMOUNT: {}\nSOL COST: {} SOL\nTIME: {}\nSIGNATURE: {}",
                                                                                mint_address,
                                                                                buy_args.amount,
                                                                                buy_args.max_sol_cost as f64 / 1_000_000_000.0,
                                                                                formatted_time,
                                                                                signature
                                                                            );
                                                                            
                                                                            // 如果启用缓存，将Buy交易缓存起来
                                                                            if let Some(cache_ref) = &cache {
                                                                                cache_ref.cache_buy_transaction(&signature, log_message.clone(), Some(&mint_address));
                                                                            }
                                                                            
                                                                            if is_monitored_address_involved {
                                                                                info!("{}", log_message);
                                                                                
                                                                                // 记录到文件
                                                                                if features.log_to_file {
                                                                                    if let Some(file) = &mut log_file {
                                                                                        // 获取当前时间戳用于日志
                                                                                        let current_time_millis = SystemTime::now()
                                                                                            .duration_since(UNIX_EPOCH)
                                                                                            .expect("Time went backwards");
                                                                                        
                                                                                        // 创建UTC时间
                                                                                        let utc_time = Utc.timestamp_millis_opt(
                                                                                            current_time_millis.as_millis() as i64
                                                                                        ).unwrap();
                                                                                        
                                                                                        // 转换为东八区（北京时间）
                                                                                        let beijing_offset = FixedOffset::east_opt(8 * 3600).unwrap();
                                                                                        let beijing_time = utc_time.with_timezone(&beijing_offset);
                                                                                        
                                                                                        // 格式化时间
                                                                                        let log_time = beijing_time.format("%Y-%m-%dT%H:%M:%S%.3f+08:00").to_string();
                                                                                        
                                                                                        let _ = writeln!(file, "[{}] {}", log_time, log_message);
                                                                                    }
                                                                                }
                                                                            } else {
                                                                                log::debug!("{}", log_message);
                                                                            }
                                                                        },
                                                                        PumpProgramIx::Sell(ref sell_args) => {
                                                                            let log_message = format!(
                                                                                "TYPE: Sell\nMINT: {}\nTOKEN AMOUNT: {}\nMIN SOL OUTPUT: {} SOL\nTIME: {}\nSIGNATURE: {}",
                                                                                mint_address,
                                                                                sell_args.amount,
                                                                                sell_args.min_sol_output as f64 / 1_000_000_000.0,
                                                                                formatted_time,
                                                                                signature
                                                                            );
                                                                            
                                                                            // 如果启用缓存，将Sell交易缓存起来
                                                                            if let Some(cache_ref) = &cache {
                                                                                cache_ref.cache_sell_transaction(&signature, log_message.clone(), Some(&mint_address));
                                                                            }
                                                                            
                                                                            if is_monitored_address_involved {
                                                                                info!("{}", log_message);
                                                                                
                                                                                // 记录到文件
                                                                                if features.log_to_file {
                                                                                    if let Some(file) = &mut log_file {
                                                                                        // 获取当前时间戳用于日志
                                                                                        let current_time_millis = SystemTime::now()
                                                                                            .duration_since(UNIX_EPOCH)
                                                                                            .expect("Time went backwards");
                                                                                        
                                                                                        // 创建UTC时间
                                                                                        let utc_time = Utc.timestamp_millis_opt(
                                                                                            current_time_millis.as_millis() as i64
                                                                                        ).unwrap();
                                                                                        
                                                                                        // 转换为东八区（北京时间）
                                                                                        let beijing_offset = FixedOffset::east_opt(8 * 3600).unwrap();
                                                                                        let beijing_time = utc_time.with_timezone(&beijing_offset);
                                                                                        
                                                                                        // 格式化时间
                                                                                        let log_time = beijing_time.format("%Y-%m-%dT%H:%M:%S%.3f+08:00").to_string();
                                                                                        
                                                                                        let _ = writeln!(file, "[{}] {}", log_time, log_message);
                                                                                    }
                                                                                }
                                                                            } else {
                                                                                log::debug!("{}", log_message);
                                                                            }
                                                                        },
                                                                        _ => {
                                                                            // 其他 PumpFun 指令
                                                                            log::debug!("检测到其他 PumpFun 指令: {}", decoded_ix.name());
                                                                        }
                                                                    }
                                                                } else {
                                                                    log::debug!("无法序列化指令为JSON");
                                                                }
                                                            } else {
                                                                log::debug!("无法映射账户");
                                                            }
                                                        } else {
                                                            // 没有IDL文件，无法映射账户和提取mint信息
                                                            match decoded_ix {
                                                                PumpProgramIx::Buy(ref buy_args) => {
                                                                    log::debug!("Buy操作 (无mint信息): Amount: {}, MaxSolCost: {}", 
                                                                        buy_args.amount, buy_args.max_sol_cost);
                                                                },
                                                                PumpProgramIx::Sell(ref sell_args) => {
                                                                    log::debug!("Sell操作 (无mint信息): Amount: {}, MinSolOutput: {}", 
                                                                        sell_args.amount, sell_args.min_sol_output);
                                                                },
                                                                _ => {
                                                                    log::debug!("其他PumpFun指令: {}", decoded_ix.name());
                                                                }
                                                            }
                                                        }
                                                    },
                                                    Err(_) => {
                                                        // 解析失败，不记录错误
                                                    }
                                                }
                                            }
                                        }
                                        
                                        // 检查是否是Token程序并且Token监控已启用
                                        if features.token_transaction_monitoring {
                                            if let Ok(token_program_pubkey) = Pubkey::from_str(TOKEN_PROGRAM_ID) {
                                                let token_program_bytes = token_program_pubkey.to_bytes().to_vec();
                                                if program_id_bytes == &token_program_bytes && is_monitored_address_involved {
                                                    // 尝试解析Token指令
                                                    match TokenInstruction::unpack(&instruction.data) {
                                                        Ok(decoded_ix) => {
                                                            let timestamp_millis = SystemTime::now()
                                                                .duration_since(UNIX_EPOCH)
                                                                .expect("Time went backwards");
                                                            
                                                            // 创建UTC时间
                                                            let utc_datetime = Utc.timestamp_millis_opt(
                                                                timestamp_millis.as_millis() as i64
                                                            ).unwrap();
                                                            
                                                            // 转换为东八区（北京时间，UTC+8）
                                                            let beijing_offset = FixedOffset::east_opt(8 * 3600).unwrap(); // 8小时 = 8 * 3600秒
                                                            let beijing_time = utc_datetime.with_timezone(&beijing_offset);
                                                            
                                                            // 格式化为ISO 8601格式，显示+08:00时区信息
                                                            let formatted_time = beijing_time.format("%Y-%m-%dT%H:%M:%S%.3f+08:00").to_string();
                                                            
                                                            let ix_name = get_instruction_name_with_typename(&decoded_ix);
                                                            let _serializable_ix = convert_to_serializable(decoded_ix);
                                                            
                                                            let log_message = format!("Token指令: {}, 时间: {}, 签名: {}", 
                                                                ix_name, 
                                                                formatted_time, 
                                                                signature);
                                                            
                                                            log::debug!("{}", log_message);
                                                            
                                                            // 记录到文件
                                                            if features.log_to_file {
                                                                if let Some(file) = &mut log_file {
                                                                    // 获取当前时间戳用于日志
                                                                    let current_time_millis = SystemTime::now()
                                                                        .duration_since(UNIX_EPOCH)
                                                                        .expect("Time went backwards");
                                                                    
                                                                    // 创建UTC时间
                                                                    let utc_time = Utc.timestamp_millis_opt(
                                                                        current_time_millis.as_millis() as i64
                                                                    ).unwrap();
                                                                    
                                                                    // 转换为东八区（北京时间）
                                                                    let beijing_offset = FixedOffset::east_opt(8 * 3600).unwrap();
                                                                    let beijing_time = utc_time.with_timezone(&beijing_offset);
                                                                    
                                                                    // 格式化时间
                                                                    let log_time = beijing_time.format("%Y-%m-%dT%H:%M:%S%.3f+08:00").to_string();
                                                                    
                                                                    let _ = writeln!(file, "[{}] {}", log_time, log_message);
                                                                }
                                                            }
                                                        },
                                                        Err(_) => {
                                                            // 解析失败，不记录错误
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Some(UpdateOneof::Ping(_)) => {
                    subscribe_tx
                        .send(SubscribeRequest {
                            ping: Some(SubscribeRequestPing { id: 1 }),
                            ..Default::default()
                        })
                        .await?;
                }
                Some(UpdateOneof::Pong(_)) => {}
                None => {
                    error!("消息中未找到更新内容");
                    break;
                }
                _ => {}
            },
            Err(error) => {
                error!("错误: {error:?}");
                break;
            }
        }
    }

    info!("数据流已关闭");
    Ok(())
}

/// 处理账户数据更新的函数
async fn geyser_subscribe_accounts(
    mut client: GeyserGrpcClient<impl Interceptor>,
    request: SubscribeRequest,
    features: &Features,
    cache: Option<Arc<TransactionCache>>,
) -> anyhow::Result<()> {
    let (mut subscribe_tx, mut stream) = client.subscribe_with_request(Some(request)).await?;

    // 打开日志文件（如果启用）
    let mut log_file = if features.log_to_file {
        Some(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&features.log_file_path)?
        )
    } else {
        None
    };

    log::debug!("账户数据流已打开");

    while let Some(message) = stream.next().await {
        match message {
            Ok(msg) => match msg.update_oneof {
                Some(UpdateOneof::Account(account)) => {
                    let slot = account.slot;
                    
                    if let Some(account_data) = account.account {
                        let pubkey_str = bs58::encode(&account_data.pubkey).into_string();
                        // 添加下划线前缀表示故意不使用的变量
                        let _owner = bs58::encode(&account_data.owner).into_string();
                        let _lamports = account_data.lamports;
                        
                        // 尝试解码账户数据
                        match decode_account_data(&account_data.data) {
                            Ok(decoded_account) => {
                                let account_info = match &decoded_account {
                                    DecodedAccount::BondingCurve(bc) => {
                                        let timestamp_millis = SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .expect("Time went backwards");
                                            
                                            // 创建UTC时间
                                            let utc_datetime = Utc.timestamp_millis_opt(
                                                timestamp_millis.as_millis() as i64
                                            ).unwrap();
                                            
                                            // 转换为东八区（北京时间，UTC+8）
                                            let beijing_offset = FixedOffset::east_opt(8 * 3600).unwrap(); // 8小时 = 8 * 3600秒
                                            let beijing_time = utc_datetime.with_timezone(&beijing_offset);
                                            
                                            // 格式化为ISO 8601格式，显示+08:00时区信息
                                            let formatted_time = beijing_time.format("%Y-%m-%dT%H:%M:%S%.3f+08:00").to_string();
                                            
                                            format!("
                                            ACCOUNT TYPE: BondingCurve
                                            PUBKEY: {}
                                            VIRTUAL TOKEN RESERVES: {}
                                            VIRTUAL SOL RESERVES: {}
                                            REAL TOKEN RESERVES: {}
                                            REAL SOL RESERVES: {}
                                            TOKEN TOTAL SUPPLY: {}
                                            COMPLETE: {}
                                            TIME: {}
                                            ",
                                            pubkey_str,
                                            bc.virtual_token_reserves,
                                            bc.virtual_sol_reserves,
                                            bc.real_token_reserves,
                                            bc.real_sol_reserves,
                                            bc.token_total_supply,
                                            bc.complete,
                                            formatted_time
                                            )
                                    },
                                    DecodedAccount::Global(global) => {
                                        let timestamp_millis = SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .expect("Time went backwards");
                                            
                                            // 创建UTC时间
                                            let utc_datetime = Utc.timestamp_millis_opt(
                                                timestamp_millis.as_millis() as i64
                                            ).unwrap();
                                            
                                            // 转换为东八区（北京时间，UTC+8）
                                            let beijing_offset = FixedOffset::east_opt(8 * 3600).unwrap(); // 8小时 = 8 * 3600秒
                                            let beijing_time = utc_datetime.with_timezone(&beijing_offset);
                                            
                                            // 格式化为ISO 8601格式，显示+08:00时区信息
                                            let formatted_time = beijing_time.format("%Y-%m-%dT%H:%M:%S%.3f+08:00").to_string();
                                            
                                            let fee_recipient = bs58::encode(&global.fee_recipient.to_bytes()).into_string();
                                            let authority = bs58::encode(&global.authority.to_bytes()).into_string();
                                            
                                            format!("
                                            ACCOUNT TYPE: Global
                                            PUBKEY: {}
                                            INITIALIZED: {}
                                            AUTHORITY: {}
                                            FEE RECIPIENT: {}
                                            INITIAL VIRTUAL TOKEN RESERVES: {}
                                            INITIAL VIRTUAL SOL RESERVES: {}
                                            INITIAL REAL TOKEN RESERVES: {}
                                            TOKEN TOTAL SUPPLY: {}
                                            FEE BASIS POINTS: {}
                                            TIME: {}
                                            ",
                                            pubkey_str,
                                            global.initialized,
                                            authority,
                                            fee_recipient,
                                            global.initial_virtual_token_reserves,
                                            global.initial_virtual_sol_reserves,
                                            global.initial_real_token_reserves,
                                            global.token_total_supply,
                                            global.fee_basis_points,
                                            formatted_time
                                            )
                                    }
                                };
                                
                                // 如果启用缓存，将账户数据添加到缓存
                                if let Some(cache_ref) = &cache {
                                    cache_ref.cache_account_data(&pubkey_str, account_info.clone());
                                }
                                
                                // 使用debug级别输出账户信息
                                log::debug!("{}", account_info);
                                
                                // 记录到文件
                                if features.log_to_file {
                                    if let Some(file) = &mut log_file {
                                        // 获取当前时间戳用于日志
                                        let current_time_millis = SystemTime::now()
                                            .duration_since(UNIX_EPOCH)
                                            .expect("Time went backwards");
                                        
                                        // 创建UTC时间
                                        let utc_time = Utc.timestamp_millis_opt(
                                            current_time_millis.as_millis() as i64
                                        ).unwrap();
                                        
                                        // 转换为东八区（北京时间）
                                        let beijing_offset = FixedOffset::east_opt(8 * 3600).unwrap();
                                        let beijing_time = utc_time.with_timezone(&beijing_offset);
                                        
                                        // 格式化时间
                                        let log_time = beijing_time.format("%Y-%m-%dT%H:%M:%S%.3f+08:00").to_string();
                                        
                                        let _ = writeln!(file, "[{}] {}", log_time, account_info);
                                    }
                                }
                            },
                            Err(e) => {
                                log::debug!("解析账户数据失败: {}", e.message);
                            }
                        }
                    } else {
                        log::debug!("账户数据为空，槽位: {}", slot);
                    }
                },
                Some(UpdateOneof::Ping(_)) => {
                    subscribe_tx
                        .send(SubscribeRequest {
                            ping: Some(SubscribeRequestPing { id: 1 }),
                            ..Default::default()
                        })
                        .await?;
                }
                Some(UpdateOneof::Pong(_)) => {}
                None => {
                    error!("消息中未找到更新内容");
                    break;
                }
                _ => {}
            },
            Err(error) => {
                error!("错误: {error:?}");
                break;
            }
        }
    }

    info!("账户数据流已关闭");
    Ok(())
}

/// 解码账户数据为特定类型
pub fn decode_account_data(buf: &[u8]) -> Result<DecodedAccount, AccountDecodeError> {
    if buf.len() < 8 {
        return Err(AccountDecodeError {
            message: "缓冲区太短，无法包含有效的鉴别器".to_string(),
        });
    }

    let discriminator: [u8; 8] = buf[..8].try_into().expect("无法提取前8个字节");

    match discriminator {
        BONDING_CURVE_ACCOUNT_DISCM => {
            let data = BondingCurveAccount::deserialize(buf)
                .map_err(|e| AccountDecodeError {
                    message: format!("无法反序列化BondingCurveAccount: {}", e),
                })?;
            log::debug!("解码的绑定曲线结构: {:#?}", data);
            Ok(DecodedAccount::BondingCurve(data.0))
        }
        GLOBAL_ACCOUNT_DISCM => {
            let data = GlobalAccount::deserialize(buf)
                .map_err(|e| AccountDecodeError {
                    message: format!("无法反序列化GlobalAccount: {}", e),
                })?;
            log::debug!("解码的全局结构: {:#?}", data);
            Ok(DecodedAccount::Global(data.0))
        }
        _ => Err(AccountDecodeError {
            message: "未找到账户的鉴别器".to_string(),
        }),
    }
}

/// 从账户数据中提取mint地址
/// 通过反向计算PDA的方式找到与绑定曲线账户关联的mint地址
fn extract_mint_address_from_account_data(account_data_str: &str) -> Option<String> {
    if account_data_str.contains("BondingCurve") {
        // 从账户数据中提取pubkey
        if let Some(pubkey_line) = account_data_str.lines().find(|line| line.trim().starts_with("PUBKEY:")) {
            let pubkey_str = pubkey_line.trim().strip_prefix("PUBKEY:").unwrap_or("").trim();
            if let Ok(curve_pubkey) = Pubkey::from_str(pubkey_str) {
                // PumpFun程序ID
                let pump_program_id = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
                if let Ok(program_id) = Pubkey::from_str(pump_program_id) {
                    // 从实际交易数据中看到的mint地址列表
                    let common_mints = [
                        "DCLjJRAP4PineCmCabTKRrTVsSaggkmfgBj8AMPapump",
                        "4qMyinhBRrePr82BjoKheaXocfTXChBMk3TWifHypump",
                        "7kJzws2KnTV73d16ZuifeFmSyupxYkp7CPYenV3Apump",
                        "FqF6Ac1j71qjTxjg9mJag3zrmmnxVtXJQTxZjSPdpump",
                        // 可以添加更多已知的mint地址
                    ];
                    
                    // 遍历已知mint地址并验证
                    for mint_str in common_mints.iter() {
                        if let Ok(mint_pubkey) = Pubkey::from_str(mint_str) {
                            // 验证PDA
                            let seeds = &[b"bonding-curve", mint_pubkey.as_ref()];
                            let (derived_pubkey, _) = Pubkey::find_program_address(seeds, &program_id);
                            
                            if derived_pubkey == curve_pubkey {
                                debug!("[PDA] 成功反向计算: 曲线账户({}) -> Mint地址({})", pubkey_str, mint_str);
                                return Some(mint_str.to_string());
                            }
                        }
                    }
                    
                    // 如果没有匹配的mint，记录日志
                    debug!("[PDA] 无法找到曲线账户({})对应的mint地址", pubkey_str);
                }
            }
        }
    }
    
    None
}

/// 从账户数据中提取虚拟储备信息
fn extract_reserves_from_account_data(account_data_str: &str) -> Option<(u64, u64)> {
    if account_data_str.contains("BondingCurve") {
        // 查找虚拟代币储备
        let vt_line = account_data_str.lines()
            .find(|line| line.trim().contains("VIRTUAL TOKEN RESERVES"));
        let vs_line = account_data_str.lines()
            .find(|line| line.trim().contains("VIRTUAL SOL RESERVES"));
        
        if let (Some(vt_line), Some(vs_line)) = (vt_line, vs_line) {
            // 提取数值
            let vt_str = vt_line.trim().split(':').last()?.trim();
            let vs_str = vs_line.trim().split(':').last()?.trim();
            
            // 尝试解析为数字
            if let (Ok(vt), Ok(vs)) = (vt_str.parse::<u64>(), vs_str.parse::<u64>()) {
                debug!("[提取] 成功提取虚拟储备 - 代币: {}, SOL: {}", vt, vs);
                return Some((vt, vs));
            } else {
                debug!("[提取] 无法解析虚拟储备数值: \"{}\" 和 \"{}\"", vt_str, vs_str);
            }
        } else {
            debug!("[提取] 账户数据中未找到虚拟储备字段");
        }
    }
    
    None
}

/// 从mint地址计算绑定曲线账户地址
fn calculate_curve_account_from_mint(mint: &str) -> Option<String> {
    // PumpFun程序ID
    let pump_program_id = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
    
    if let (Ok(mint_pubkey), Ok(program_id)) = (Pubkey::from_str(mint), Pubkey::from_str(pump_program_id)) {
        // 使用mint地址和程序ID计算PDA
        let seeds = &[b"bonding-curve", mint_pubkey.as_ref()];
        let (derived_pubkey, _) = Pubkey::find_program_address(seeds, &program_id);
        
        // 返回计算出的账户地址
        let curve_account = derived_pubkey.to_string();
        debug!("[PDA] 从Mint({})计算出曲线账户({})", mint, curve_account);
        return Some(curve_account);
    }
    
    None
}