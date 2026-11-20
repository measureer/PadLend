/** 版本号四处一致（package.json / src-tauri/tauri.conf.json / 两个 Cargo.toml），由 release.yml 发版强检；前端从 package.json 读取。 */
import { version as PACKAGE_VERSION } from '../../package.json';

export const APP_VERSION = PACKAGE_VERSION;
