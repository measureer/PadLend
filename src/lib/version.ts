/** 应用版本只有一个来源：package.json（src-tauri/tauri.conf.json 与 release.ps1 也从这里同步）。 */
import { version as PACKAGE_VERSION } from '../../package.json';

export const APP_VERSION = PACKAGE_VERSION;
