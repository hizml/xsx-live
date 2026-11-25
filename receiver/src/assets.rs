// 播放页的静态文件编进程序，启动时写到分发服务（SRS）读取的目录。
// 这样镜像自带这些文件，装好之后配置目录会自动生成，不用提前放；镜像升级时页面也跟着更新
use std::{io::Write, path::Path};

// 播放页用到的库是 xqq/mpegts.js 1.8.2（MIT），来源和校验值见 vendor/SOURCE.json，许可证随文件一起分发
pub const WWW_FILES: [(&str, &[u8]); 4] = [
    ("index.html", include_bytes!("../../www/index.html")),
    ("vendor/mpegts-1.8.2.js", include_bytes!("../../www/vendor/mpegts-1.8.2.js")),
    ("vendor/LICENSE", include_bytes!("../../www/vendor/LICENSE")),
    ("vendor/SOURCE.json", include_bytes!("../../www/vendor/SOURCE.json")),
];

// 把播放页的文件写进目录。内容一样的不动；不一样的先写临时文件再改名，分发服务不会读到写了一半的文件
pub fn install_www(dir: &Path) -> std::io::Result<()> {
    for (name, body) in WWW_FILES {
        let path = dir.join(name);
        if std::fs::read(&path).is_ok_and(|current| current == body) { continue; }
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        let tmp = path.with_extension("tmp");
        std::fs::File::create(&tmp)?.write_all(body)?;
        std::fs::rename(&tmp, &path)?;
        crate::log!("WWW_FILE installed name={name} bytes={}", body.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = WWW_FILES.iter().map(|(name, _)| name.to_string()).filter(|name| dir.join(name).is_file()).collect();
        found.sort();
        found
    }

    #[test]
    fn player_page_references_only_files_that_ship_with_it() {
        let page = String::from_utf8_lossy(WWW_FILES[0].1).to_string();
        assert!(page.contains("src=\"vendor/mpegts-1.8.2.js\""), "播放页引用的库要一起带上去");
        for (name, body) in WWW_FILES { assert!(!body.is_empty(), "{name} 不应该是空的"); }
    }

    #[test]
    fn installs_missing_or_outdated_files_and_keeps_matching_ones() {
        let dir = std::env::temp_dir().join(format!("xsx-www-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vendor")).unwrap();
        // 一个过期的页面、一个内容相同的库，另外两个文件缺失
        std::fs::write(dir.join("index.html"), "old page").unwrap();
        std::fs::write(dir.join("vendor/mpegts-1.8.2.js"), WWW_FILES[1].1).unwrap();
        install_www(&dir).unwrap();
        assert_eq!(std::fs::read(dir.join("index.html")).unwrap(), WWW_FILES[0].1, "过期的页面应该被换成新的");
        let expected: Vec<String> = ["index.html", "vendor/LICENSE", "vendor/SOURCE.json", "vendor/mpegts-1.8.2.js"].iter().map(|name| name.to_string()).collect();
        assert_eq!(names(&dir), expected);
        assert!(!dir.join("index.tmp").exists(), "临时文件不该留下来");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_the_directory_tree_on_a_fresh_install() {
        let dir = std::env::temp_dir().join(format!("xsx-www-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        install_www(&dir.join("not/there/yet")).unwrap();
        assert_eq!(names(&dir.join("not/there/yet")).len(), WWW_FILES.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
