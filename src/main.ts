import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";

const selectFolderBtn = document.getElementById("selectFolderBtn") as HTMLButtonElement;
const selectedPath = document.getElementById("selectedPath") as HTMLDivElement;
const serverSection = document.getElementById("serverSection") as HTMLDivElement;
const qrCode = document.getElementById("qrCode") as HTMLDivElement;
const serverUrl = document.getElementById("serverUrl") as HTMLDivElement;
const stopServerBtn = document.getElementById("stopServerBtn") as HTMLButtonElement;

let currentPath: string | null = null;

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
        }
    } catch (error) {
        console.error("Error selecting folder:", error);
    }
});

stopServerBtn.addEventListener("click", async () => {
    try {
        await invoke("stop_server");
        serverSection.style.display = "none";
        currentPath = null;
    } catch (error) {
        console.error("Error stopping server:", error);
    }
});

async function startServer(folderPath: string) {
    try {
        qrCode.innerHTML = '<div class="loading">正在启动服务...</div>';
        serverSection.style.display = "block";

        const serverInfo = await invoke<{ url: string; port: number }>("start_server", {
            folderPath
        });

        serverUrl.textContent = serverInfo.url;

        const qrDataUrl = await invoke<string>("generate_qr_code", {
            url: serverInfo.url
        });

        qrCode.innerHTML = `<img src="${qrDataUrl}" alt="QR Code">`;
    } catch (error) {
        console.error("Error starting server:", error);
        qrCode.innerHTML = '<div class="error">启动失败，请重试</div>';
    }
}
