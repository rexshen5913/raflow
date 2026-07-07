//! 手動測試輔助：直接彈出 D1「教 raflow 一個更正」popover（免跑整條語音管線）。
//!
//! 執行：`cargo run --example popover_demo -p raflow-app`
//! 目的：肉眼驗證下拉候選、可編輯「正確」欄、「也加優先區」勾選、記住/取消，以及「記住」
//! 回傳的 heard/correct/add_to_priority 值是否正確。**不寫任何檔案**（純 UI 驗證）。
//!
//! 這是測試輔助，非產品碼；用 `#[path]` 直接引入 bin 的 popover 模組以免重複。

// 本 demo 只用 `prompt_correction`；同模組的 `show_notice` 等在正式 binary 才被呼叫，
// 於此 example 編譯單元中未用到 → 允許 dead_code，不影響正式 binary 的偵測。
#[path = "../src/correction_popover.rs"]
#[allow(dead_code)]
mod correction_popover;

fn main() {
    let candidates = vec![
        "Terraform".to_string(),
        "Ansible".to_string(),
        "K8S".to_string(),
    ];
    println!("彈出 popover（候選：{candidates:?}）——請操作對話框…");
    match correction_popover::prompt_correction(&candidates) {
        Some(input) => println!(
            "✅ 記住 → heard={:?} correct={:?} add_to_priority={}",
            input.heard, input.correct, input.add_to_priority
        ),
        None => println!("↩️  取消（未記住，或非主執行緒）"),
    }
}
