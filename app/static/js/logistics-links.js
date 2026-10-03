/**
 * 物流链接管理页
 * 列出每个物流公司的专属链接, 支持复制/重置, 以及手动触发发货单快照同步。
 */

(function () {
    'use strict';

    const API_BASE = '/api/admin/logistics-links';

    const els = {
        syncStatusText: document.getElementById('syncStatusText'),
        tableBody: document.getElementById('linksTableBody'),
        btnSync: document.getElementById('btnSync'),
        toast: document.getElementById('toast')
    };

    let toastTimer = null;

    function showToast(message) {
        els.toast.textContent = message;
        els.toast.classList.add('show');
        clearTimeout(toastTimer);
        toastTimer = setTimeout(() => els.toast.classList.remove('show'), 2500);
    }

    function escapeHtml(text) {
        const div = document.createElement('div');
        div.textContent = text == null ? '' : String(text);
        return div.innerHTML;
    }

    function formatTime(value) {
        if (!value) return '—';
        return String(value).replace('T', ' ').slice(0, 19);
    }

    function renderSyncStatus(data) {
        const status = data.sync_status;
        let statusHtml;
        if (status === 'success') {
            statusHtml = '<span class="status-ok">成功</span>';
        } else if (status === 'failed') {
            statusHtml = '<span class="status-failed">失败</span>';
        } else {
            statusHtml = '<span class="status-running">尚未同步</span>';
        }
        els.syncStatusText.innerHTML =
            `上次同步: ${escapeHtml(formatTime(data.last_sync_at))} · 状态: ${statusHtml}`;
    }

    function renderLinks(links) {
        if (!links || links.length === 0) {
            els.tableBody.innerHTML =
                '<tr><td colspan="5" class="empty-cell">暂无物流链接，等待首轮数据同步完成后自动生成</td></tr>';
            return;
        }

        els.tableBody.innerHTML = links.map(link => {
            const fullUrl = location.origin + link.link_path;
            const badgeClass = link.pending_count > 0 ? 'pending-badge' : 'pending-badge zero';
            return `
                <tr>
                    <td>${escapeHtml(link.logistics_name)}</td>
                    <td class="col-count"><span class="${badgeClass}">${link.pending_count}</span></td>
                    <td class="link-cell" title="${escapeHtml(fullUrl)}">${escapeHtml(fullUrl)}</td>
                    <td class="col-access">${escapeHtml(formatTime(link.last_access_at))}</td>
                    <td class="col-actions">
                        <button class="btn-mini" data-action="copy" data-url="${escapeHtml(fullUrl)}">复制链接</button>
                        <button class="btn-mini danger" data-action="regenerate" data-id="${link.id}"
                                data-name="${escapeHtml(link.logistics_name)}">重置链接</button>
                    </td>
                </tr>`;
        }).join('');
    }

    async function loadLinks() {
        try {
            const resp = await fetch(API_BASE + '/');
            if (!resp.ok) throw new Error('HTTP ' + resp.status);
            const data = await resp.json();
            renderSyncStatus(data);
            renderLinks(data.links);
        } catch (err) {
            els.tableBody.innerHTML =
                `<tr><td colspan="5" class="empty-cell">加载失败: ${escapeHtml(err.message)}</td></tr>`;
        }
    }

    function copyText(text) {
        // 内网HTTP为非安全上下文, Clipboard API 不可用时降级到 execCommand
        if (navigator.clipboard && window.isSecureContext) {
            return navigator.clipboard.writeText(text);
        }
        return new Promise((resolve, reject) => {
            const textarea = document.createElement('textarea');
            textarea.value = text;
            textarea.style.position = 'fixed';
            textarea.style.opacity = '0';
            document.body.appendChild(textarea);
            textarea.select();
            try {
                document.execCommand('copy') ? resolve() : reject(new Error('复制命令失败'));
            } catch (err) {
                reject(err);
            } finally {
                document.body.removeChild(textarea);
            }
        });
    }

    async function regenerateLink(linkId, name) {
        if (!confirm(`确定重置「${name}」的专属链接吗？\n旧链接将立即失效，需要重新发送新链接给物流公司。`)) {
            return;
        }
        try {
            const resp = await fetch(`${API_BASE}/${linkId}/regenerate`, { method: 'POST' });
            if (!resp.ok) {
                const body = await resp.json().catch(() => ({}));
                throw new Error(body.detail || ('HTTP ' + resp.status));
            }
            showToast('链接已重置');
            await loadLinks();
        } catch (err) {
            showToast('重置失败: ' + err.message);
        }
    }

    async function pollSyncUntilDone() {
        for (;;) {
            await new Promise(r => setTimeout(r, 2000));
            try {
                const resp = await fetch(API_BASE + '/sync-status');
                const status = await resp.json();
                if (!status.running) return status;
            } catch (err) {
                return null;
            }
        }
    }

    async function triggerSync() {
        els.btnSync.disabled = true;
        els.btnSync.textContent = '同步中...';
        try {
            const resp = await fetch(API_BASE + '/sync', { method: 'POST' });
            if (resp.status === 409) {
                const body = await resp.json().catch(() => ({}));
                showToast(body.detail || '同步正在进行中');
                return;
            }
            if (!resp.ok) throw new Error('HTTP ' + resp.status);

            const status = await pollSyncUntilDone();
            if (status && status.last_status === 'success') {
                showToast(`同步完成，快照共 ${status.record_count} 条`);
            } else {
                showToast('同步失败: ' + ((status && status.last_error) || '未知错误'));
            }
            await loadLinks();
        } catch (err) {
            showToast('同步失败: ' + err.message);
        } finally {
            els.btnSync.disabled = false;
            els.btnSync.textContent = '立即同步';
        }
    }

    els.tableBody.addEventListener('click', (event) => {
        const button = event.target.closest('button[data-action]');
        if (!button) return;
        if (button.dataset.action === 'copy') {
            copyText(button.dataset.url)
                .then(() => showToast('链接已复制'))
                .catch(() => showToast('复制失败，请手动选择复制'));
        } else if (button.dataset.action === 'regenerate') {
            regenerateLink(button.dataset.id, button.dataset.name);
        }
    });

    els.btnSync.addEventListener('click', triggerSync);

    loadLinks();
})();
