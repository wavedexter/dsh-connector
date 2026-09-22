use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// 家里服务器地址:IPv6 或域名
    pub host: String,
    pub ssh_port: u16,
    pub user: String,
    /// 私钥文件路径(在运行本 App 的电脑上)
    pub key_path: String,
    /// 本地固定端口(硬约束:cookie 绑 authority,不能随机)
    pub local_port: u16,
    pub remote_port: u16,
    /// 服务器上 dsh 日志路径(token 从这里 grep)
    pub dsh_log_path: String,
    pub auto_connect: bool,
    /// 界面主题:"dark" | "light"
    #[serde(default)]
    pub theme: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: String::new(),
            ssh_port: 22,
            user: String::new(),
            key_path: String::new(),
            local_port: 18080,
            remote_port: 3080,
            dsh_log_path: String::new(),
            auto_connect: true,
            theme: "dark".to_string(),
        }
    }
}
