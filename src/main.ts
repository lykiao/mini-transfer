import { invoke } from "@tauri-apps/api/core";
import { getVersion } from "@tauri-apps/api/app";
import { open } from "@tauri-apps/plugin-dialog";
import { open as shellOpen } from "@tauri-apps/plugin-shell";
import { listen } from "@tauri-apps/api/event";

const selectFolderBtn = document.getElementById("selectFolderBtn") as HTMLButtonElement;
const selectedPath = document.getElementById("selectedPath") as HTMLDivElement;
const serverSection = document.getElementById("serverSection") as HTMLDivElement;
const qrCode = document.getElementById("qrCode") as HTMLDivElement;
const serverUrl = document.getElementById("serverUrl") as HTMLDivElement;
const serverToggleBtn = document.getElementById("serverToggleBtn") as HTMLButtonElement;
const versionBadge = document.getElementById("versionBadge") as HTMLSpanElement;
const copyPathBtn = document.getElementById("copyPathBtn") as HTMLButtonElement;
const openFolderBtn = document.getElementById("openFolderBtn") as HTMLButtonElement;
const copyUrlBtn = document.getElementById("copyUrlBtn") as HTMLButtonElement;

let currentPath: string | null = null;
let isServerRunning = false;

// 按钮状态完全由 currentPath / isServerRunning 驱动，与"是否初次启动"无关
function updateToggleButton() {
    if (isServerRunning) {
        serverToggleBtn.textContent = '停止服务';
        serverToggleBtn.className = 'btn danger';
        serverToggleBtn.disabled = false;
    } else if (currentPath) {
        serverToggleBtn.textContent = '启动服务';
        serverToggleBtn.className = 'btn success';
        serverToggleBtn.disabled = false;
    } else {
        serverToggleBtn.textContent = '启动服务';
        serverToggleBtn.className = 'btn success';
        serverToggleBtn.disabled = true;
    }
}

// 在标题栏显示当前应用版本号
getVersion()
    .then((version) => { versionBadge.textContent = `v${version}`; })
    .catch(() => { versionBadge.textContent = ''; });

// 复制文本到剪贴板（含降级方案，兼容不同 WebView 环境）
async function copyText(text: string, successMsg: string) {
    try {
        if (navigator.clipboard?.writeText) {
            await navigator.clipboard.writeText(text);
        } else {
            // 降级：临时 textarea + execCommand
            const ta = document.createElement('textarea');
            ta.value = text;
            ta.style.cssText = 'position:fixed;opacity:0;left:-9999px;';
            document.body.appendChild(ta);
            ta.select();
            document.execCommand('copy');
            document.body.removeChild(ta);
        }
        showToast(successMsg);
    } catch (e) {
        console.error('复制失败:', e);
        showToast('复制失败');
    }
}

copyPathBtn.addEventListener('click', () => {
    if (!currentPath) {
        showToast('请先选择共享文件夹');
        return;
    }
    copyText(currentPath, '路径已复制');
});

openFolderBtn.addEventListener('click', async () => {
    if (!currentPath) {
        showToast('请先选择共享文件夹');
        return;
    }
    try {
        await shellOpen(currentPath);
    } catch (e) {
        console.error('打开文件夹失败:', e);
        showToast('打开文件夹失败');
    }
});

copyUrlBtn.addEventListener('click', () => {
    const url = serverUrl.textContent?.trim();
    if (!url) return;
    copyText(url, '链接已复制');
});

// 记住上次成功启动服务的共享文件夹路径
const LAST_FOLDER_KEY = 'mini-transfer:lastFolder';

function saveLastFolder(path: string) {
    try {
        localStorage.setItem(LAST_FOLDER_KEY, path);
    } catch (e) {
        console.warn('无法保存上次文件夹:', e);
    }
}

function clearLastFolder() {
    try {
        localStorage.removeItem(LAST_FOLDER_KEY);
    } catch (e) {
        console.warn('无法清除上次文件夹:', e);
    }
}

async function restoreLastFolder() {
    let savedPath: string | null = null;
    try {
        savedPath = localStorage.getItem(LAST_FOLDER_KEY);
    } catch (e) {
        console.warn('无法读取上次文件夹:', e);
        return;
    }
    if (!savedPath) return;

    // 前端无法可靠判断本机路径是否存在，交给后端校验
    let stillExists = false;
    try {
        stillExists = await invoke<boolean>('validate_folder', { folderPath: savedPath });
    } catch (e) {
        console.error('校验上次文件夹失败:', e);
    }

    if (stillExists) {
        currentPath = savedPath;
        selectedPath.textContent = savedPath;
        showToast('已恢复上次的共享文件夹');
        await startServer(savedPath);
    } else {
        // 原文件夹已被删除或移动，清空记录并显示空
        clearLastFolder();
        selectedPath.textContent = '未选择';
        showToast('上次的共享文件夹已不存在，请重新选择');
    }
}

// Listen for file upload events
listen<{ filename: string; size: number }>('file-received', (event) => {
    const { filename, size } = event.payload;
    const sizeStr = formatSize(size);
    showToast(`📱 收到文件：${filename} (${sizeStr})`);
});

function formatSize(bytes: number): string {
    if (bytes < 1024) return bytes + ' B';
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
    return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
}

function showToast(message: string) {
    const toast = document.createElement('div');
    toast.className = 'toast-notification';
    toast.textContent = message;
    toast.style.cssText = `
        position: fixed;
        top: 20px;
        left: 50%;
        transform: translateX(-50%);
        background: rgba(0, 0, 0, 0.85);
        color: white;
        padding: 12px 24px;
        border-radius: 25px;
        font-size: 14px;
        z-index: 1000;
        box-shadow: 0 4px 12px rgba(0,0,0,0.15);
        animation: slideDown 0.3s ease-out;
    `;
    document.body.appendChild(toast);

    setTimeout(() => {
        toast.style.animation = 'slideUp 0.3s ease-out';
        setTimeout(() => toast.remove(), 300);
    }, 3000);
}

selectFolderBtn.addEventListener("click", async () => {
    try {
        const selected = await open({
            directory: true,
            multiple: false,
            title: "选择要共享的文件夹"
        });

        if (selected && typeof selected === "string") {
            currentPath = selected;
            selectedPath.textContent = selected;
            await startServer(selected);
            // 无论启动是否成功都记住选择；若路径无效下次启动会自动清理
            saveLastFolder(selected);
        }
    } catch (error) {
        console.error("Error selecting folder:", error);
    }
});

serverToggleBtn.addEventListener("click", async () => {
    if (isServerRunning) {
        // 停止服务：只清空二维码与 URL 内容，保留文件夹和区域，方便再次启动
        try {
            await invoke("stop_server");
            isServerRunning = false;
            qrCode.innerHTML = '<div class="stopped">服务已停止</div>';
            serverUrl.textContent = '';
            showToast('服务已停止，可随时重新启动');
            updateToggleButton();
        } catch (error) {
            console.error("Error stopping server:", error);
        }
    } else {
        // 重新启动：复用当前选中的文件夹，无需重启应用
        if (!currentPath) {
            showToast('请先选择共享文件夹');
            return;
        }
        await startServer(currentPath);
    }
});

async function startServer(folderPath: string) {
    try {
        qrCode.innerHTML = '<div class="loading">正在启动服务...</div>';
        serverSection.style.display = "block";
        serverToggleBtn.disabled = true;

        const serverInfo = await invoke<{ url: string; port: number }>("start_server", {
            folderPath
        });

        serverUrl.textContent = serverInfo.url;

        const qrDataUrl = await invoke<string>("generate_qr_code", {
            url: serverInfo.url
        });

        qrCode.innerHTML = `<img src="${qrDataUrl}" alt="QR Code">`;
        isServerRunning = true;
    } catch (error) {
        console.error("Error starting server:", error);
        qrCode.innerHTML = '<div class="error">启动失败，请重试</div>';
    } finally {
        // 按当前状态恢复：运行中→红色「停止服务」，有路径未运行→绿色「启动服务」可重试
        updateToggleButton();
    }
}

// 先按当前状态刷新按钮，再尝试恢复上次的共享文件夹（恢复成功会自动启动服务）
updateToggleButton();
restoreLastFolder();
