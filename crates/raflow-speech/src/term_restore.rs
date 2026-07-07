//! Apple 術語還原（spec/whisper.md §18）：滾動定稿後處理。
//!
//! Whisper 定稿是「重新轉錄、整句取代」——Apple 靠 contextualStrings 認對的術語
//! 可能被 Whisper 改壞（`ArgoCD`→`R5CT`）。本模組把 Whisper 輸出與同段音訊的
//! Apple 草稿做英文詞段對齊，Apple 側命中詞彙表的術語若 Whisper 側不同 → 還原
//! Apple 版本；中文與標點一律維持 Whisper。
//!
//! 守門原則：**寧可不還原，不可還原錯**（詞段數不足、錨點不符 → 原句返回）。
//! 全部純函式，正確性由真實 live log 錯誤配對的參數化測試承擔（離線 harness
//! 無 Apple 流，無法端到端模擬）。

/// 英文詞段：`[A-Za-z0-9]` 核心、允許內部連接字元 `./+#-`（`CI/CD`、`Node.js`）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    start: usize,
    end: usize, // byte range（半開）
    text: String,
}

/// Apple 側對齊單元：一個或多個相鄰詞段（以單一空白相連且合併後命中詞彙表）。
#[derive(Debug)]
struct Unit {
    text: String,    // Apple 原文（含詞間空白）
    vocab_hit: bool, // 是否命中詞彙表（命中才有資格還原）
}

/// 抽出英文詞段：核心為 ASCII 英數，內部允許 `./+#-`（兩側須為英數，避免把
/// 句尾標點吸進來）。
fn extract_runs(text: &str) -> Vec<Run> {
    let bytes = text.as_bytes();
    let is_core = |b: u8| b.is_ascii_alphanumeric();
    let is_conn = |b: u8| matches!(b, b'.' | b'/' | b'+' | b'#' | b'-');
    let mut runs = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_core(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        while end < bytes.len() {
            if is_core(bytes[end]) {
                end += 1;
            } else if is_conn(bytes[end]) && end + 1 < bytes.len() && is_core(bytes[end + 1]) {
                end += 2;
            } else {
                break;
            }
        }
        runs.push(Run {
            start,
            end,
            text: text[start..end].to_string(),
        });
        i = end;
    }
    runs
}

/// 小寫英數投影（比對用：忽略大小寫、空白與連接字元）。
fn projection(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Levenshtein 編輯距離（比對投影字串；詞段短，O(n·m) 足夠）。
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let v = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
            cur.push(v);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Apple 側：把相鄰詞段合併為詞彙單元（合併後以單一空白相連且命中詞彙表，
/// 最長優先，上限 3 段），其餘詞段各自成非詞彙單元（對齊錨點）。
fn apple_units(draft: &str, runs: &[Run], vocab: &[String]) -> Vec<Unit> {
    let vocab_proj: Vec<String> = vocab.iter().map(|v| projection(v)).collect();
    let hit = |s: &str| {
        vocab_proj
            .iter()
            .any(|v| !v.is_empty() && *v == projection(s))
    };
    let mut units = Vec::new();
    let mut i = 0;
    while i < runs.len() {
        let mut taken = 1;
        let mut unit_text = runs[i].text.clone();
        let mut vocab_hit = hit(&unit_text);
        // 最長優先嘗試合併 3 段、2 段（詞間必須是單一空白）。
        for span in (2..=3.min(runs.len() - i)).rev() {
            let joined_ok =
                (i..i + span - 1).all(|k| &draft[runs[k].end..runs[k + 1].start] == " ");
            if !joined_ok {
                continue;
            }
            let joined = &draft[runs[i].start..runs[i + span - 1].end];
            if hit(joined) {
                taken = span;
                unit_text = joined.to_string();
                vocab_hit = true;
                break;
            }
        }
        units.push(Unit {
            text: unit_text,
            vocab_hit,
        });
        i += taken;
    }
    units
}

/// DP：把 whisper 詞段切成 `k` 個連續非空群組，最小化各組投影與對應單元投影的
/// 編輯距離總和。回傳每單元對應的 (首段 idx, 段數)。
fn align_groups(units: &[Unit], runs: &[Run]) -> Option<Vec<(usize, usize)>> {
    let k = units.len();
    let m = runs.len();
    if m < k || k == 0 {
        return None;
    }
    let unit_proj: Vec<String> = units.iter().map(|u| projection(&u.text)).collect();
    let run_proj: Vec<String> = runs.iter().map(|r| projection(&r.text)).collect();
    let group_cost = |unit: usize, from: usize, len: usize| -> usize {
        let joined: String = run_proj[from..from + len].concat();
        levenshtein(&unit_proj[unit], &joined)
    };
    // dp[u][j] = 前 u 個單元吃掉前 j 個詞段的最小成本；choice 記錄群組長度。
    const INF: usize = usize::MAX / 2;
    let mut dp = vec![vec![INF; m + 1]; k + 1];
    let mut choice = vec![vec![0usize; m + 1]; k + 1];
    dp[0][0] = 0;
    for u in 1..=k {
        for j in u..=m {
            for len in 1..=j - (u - 1) {
                let prev = dp[u - 1][j - len];
                if prev == INF {
                    continue;
                }
                let c = prev + group_cost(u - 1, j - len, len);
                if c < dp[u][j] {
                    dp[u][j] = c;
                    choice[u][j] = len;
                }
            }
        }
    }
    if dp[k][m] == INF {
        return None;
    }
    // 回溯
    let mut out = vec![(0usize, 0usize); k];
    let mut j = m;
    for u in (1..=k).rev() {
        let len = choice[u][j];
        out[u - 1] = (j - len, len);
        j -= len;
    }
    Some(out)
}

/// 主入口：以 Apple 草稿還原 Whisper 短語中被改壞的詞彙表術語。
/// 還原失敗條件（守門，spec §18）一律返回原 `whisper_phrase`。
pub fn restore_terms(whisper_phrase: &str, apple_draft: &str, vocab: &[String]) -> String {
    let original = whisper_phrase.to_string();
    if apple_draft.trim().is_empty() {
        return original;
    }
    let a_runs = extract_runs(apple_draft);
    if a_runs.is_empty() {
        return original;
    }
    let units = apple_units(apple_draft, &a_runs, vocab);
    if !units.iter().any(|u| u.vocab_hit) {
        return original; // 無詞彙命中 → 無事可還原
    }
    let w_runs = extract_runs(whisper_phrase);
    let Some(groups) = align_groups(&units, &w_runs) else {
        return original; // 詞段數不足 → 放棄
    };
    let anchors: Vec<usize> = (0..units.len()).filter(|&i| !units[i].vocab_hit).collect();
    // whisper 詞段多於單元數時，必須有錨點可驗證對齊，否則放棄（不可吞掉多出的英文）。
    if w_runs.len() > units.len() && anchors.is_empty() {
        return original;
    }
    // 錨點檢查：非詞彙單元兩側投影必須一致，否則對齊不可信。
    for &i in &anchors {
        let (from, len) = groups[i];
        let joined: String = w_runs[from..from + len]
            .iter()
            .map(|r| projection(&r.text))
            .collect();
        if joined != projection(&units[i].text) {
            return original;
        }
    }
    // 還原：詞彙單元對應的 whisper span 與 Apple 原文不同 → 取代（只動英文 span）。
    let vocab_proj: Vec<String> = vocab.iter().map(|v| projection(v)).collect();
    let mut edits: Vec<(usize, usize, &str)> = Vec::new();
    for (i, unit) in units.iter().enumerate() {
        if !unit.vocab_hit {
            continue;
        }
        let (from, len) = groups[i];
        let span_start = w_runs[from].start;
        let span_end = w_runs[from + len - 1].end;
        let span_text = &whisper_phrase[span_start..span_end];
        if span_text == unit.text {
            continue;
        }
        // Whisper 側本身命中「不同的」詞彙術語 → 兩邊各執一詞，不還原
        // （Apple 可能才是聽錯的那個；prompt priming 也可能正是讓 Whisper
        // 拼對的原因）。同一術語僅大小寫/連寫差異（投影相等）仍照常還原。
        let span_proj = projection(span_text);
        let unit_proj = projection(&unit.text);
        if span_proj != unit_proj && vocab_proj.iter().any(|v| !v.is_empty() && *v == span_proj) {
            continue;
        }
        edits.push((span_start, span_end, unit.text.as_str()));
    }
    if edits.is_empty() {
        return original;
    }
    let mut result = String::with_capacity(whisper_phrase.len());
    let mut cursor = 0;
    for (start, end, replacement) in edits {
        result.push_str(&whisper_phrase[cursor..start]);
        result.push_str(replacement);
        cursor = end;
    }
    result.push_str(&whisper_phrase[cursor..]);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vocab() -> Vec<String> {
        [
            "ArgoCD",
            "GitLab CI",
            "Terraform",
            "Vault",
            "CI/CD",
            "IaC",
            "Secret Manager",
            "Kubernetes",
            "Docker",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    /// 真實 live log 錯誤配對（2026-07-03 / 07-06 實測）為主的參數化案例：
    /// (whisper 定稿, apple 草稿, 期望輸出)
    #[test]
    fn restore_terms_recovers_vocab_hits_and_guards_misalignment() {
        let cases: &[(&str, &str, &str)] = &[
            // === 還原成功（live 實測配對）===
            // 三術語全錯（詞段數相同 1:1）
            (
                "我們用11CI搭配R5CT做XHD部署。",
                "我們用GitLab CI搭配ArgoCD做CI/CD部署",
                "我們用GitLab CI搭配ArgoCD做CI/CD部署。",
            ),
            // Telephone→Terraform；非詞彙錨點 state 大小寫不同仍算一致；
            // Secret Manager 分裂詞段（whisper 側 2 段 ↔ apple 側 1 單元）
            (
                "Telephone的State存在Vault後面的Secret Manager。",
                "Terraform的state存在Vault後面的Secret Manager",
                "Terraform的State存在Vault後面的Secret Manager。",
            ),
            // 單一術語發音級誤認
            (
                "Teraphone是我們的IaC工具",
                "Terraform是我們的IaC工具",
                "Terraform是我們的IaC工具",
            ),
            // 大小寫還原（whisper 全小寫 → 還原 Apple 的標準拼寫）
            ("我們用argocd部署", "我們用ArgoCD部署", "我們用ArgoCD部署"),
            // 已正確 → 原樣（不得畫蛇添足）
            (
                "我們用GitLab CI搭配ArgoCD部署",
                "我們用GitLab CI搭配ArgoCD部署",
                "我們用GitLab CI搭配ArgoCD部署",
            ),
            // === 守門（不得誤還原）===
            // whisper 側英文詞段少於 apple 單元數 → 放棄
            ("我們用的部署", "我們用ArgoCD部署", "我們用的部署"),
            // apple 草稿無詞彙命中 → 原樣（Foo 不在詞彙表）
            ("我們用Full部署", "我們用Foo部署", "我們用Full部署"),
            // 錨點不符（非詞彙英文兩側不一致 → 對齊不可信）→ 放棄
            ("使用Space與R5CT", "使用state與ArgoCD", "使用Space與R5CT"),
            // whisper 多出英文且無錨點可驗 → 放棄（不可吞掉 and Docker）
            (
                "用R5CT and Podman部署",
                "用ArgoCD部署",
                "用R5CT and Podman部署",
            ),
            // apple 草稿空 → 原樣
            ("我們用R5CT部署", "", "我們用R5CT部署"),
            // whisper 側本身命中「不同的」詞彙術語 → 兩邊各執一詞，不還原
            // （Apple 可能才是聽錯的那個——不可拿 Apple 的 Kubernetes 蓋掉
            // Whisper 正確的 Docker）
            (
                "我們用Docker部署",
                "我們用Kubernetes部署",
                "我們用Docker部署",
            ),
            // 多詞術語同理：Whisper 的 GitLab CI（詞彙命中）不可被 Apple 的
            // 另一術語蓋掉
            (
                "我們用GitLab CI部署",
                "我們用Terraform部署",
                "我們用GitLab CI部署",
            ),
            // 中文與標點永遠維持 whisper 側（還原只動英文 span）
            (
                "我們用R5CT，做部署。",
                "我們用ArgoCD做不熟",
                "我們用ArgoCD，做部署。",
            ),
        ];
        for (whisper, apple, expected) in cases.iter().copied() {
            assert_eq!(
                restore_terms(whisper, apple, &vocab()),
                expected,
                "restore_terms({whisper:?}, {apple:?})"
            );
        }
    }
}
