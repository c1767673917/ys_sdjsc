/**
 * 物流待上传门户页
 * 通过专属token链接访问, 展示该物流公司还未上传回单的发货单,
 * 每条提供"上传回单"入口跳转到前台上传页。
 */

(function () {
    'use strict';

    // 路径形如 /l/{token}
    const token = location.pathname.split('/').filter(Boolean).pop();

    const els = {
        companyName: document.getElementById('companyName'),
        metaText: document.getElementById('metaText'),
        btnRefresh: document.getElementById('btnRefresh'),
        searchInput: document.getElementById('searchInput'),
        listContainer: document.getElementById('listContainer')
    };

    let allDeliveries = [];

    function escapeHtml(text) {
        const div = document.createElement('div');
        div.textContent = text == null ? '' : String(text);
        return div.innerHTML;
    }

    function formatTime(value) {
        if (!value) return '—';
        return String(value).replace('T', ' ').slice(0, 16);
    }

    function formatQty(value) {
        if (value == null) return '—';
        const num = Number(value);
        if (!isFinite(num)) return '—';
        // 整数不带小数位, 小数保留原值
        return String(num);
    }

    function showState(message, extraClass) {
        els.listContainer.innerHTML =
            `<div class="state-tip ${extraClass || ''}">${escapeHtml(message)}</div>`;
    }

    function renderList() {
        const keyword = els.searchInput.value.trim().toLowerCase();
        const filtered = keyword
            ? allDeliveries.filter(d =>
                (d.delivery_code || '').toLowerCase().includes(keyword) ||
                (d.customer_name || '').toLowerCase().includes(keyword))
            : allDeliveries;

        if (allDeliveries.length === 0) {
            showState('当前没有待上传的单据，感谢配合！', 'success');
            return;
        }
        if (filtered.length === 0) {
            showState('没有匹配的单据');
            return;
        }

        els.listContainer.innerHTML = filtered.map(d => `
            <div class="delivery-card">
                <div class="delivery-info">
                    <div class="delivery-code">${escapeHtml(d.delivery_code || d.delivery_id)}</div>
                    <div class="delivery-sub">
                        <span>${escapeHtml(d.customer_name || '—')}</span>
                        <span>发货日期 ${escapeHtml(d.vouchdate || '—')}</span>
                        <span>总计价数量 ${escapeHtml(formatQty(d.total_price_qty))}</span>
                    </div>
                    ${d.shipping_memo ? `<div class="delivery-memo">${escapeHtml(d.shipping_memo)}</div>` : ''}
                </div>
                <a class="btn-upload" href="${escapeHtml(d.upload_url)}" target="_blank" rel="noopener">上传回单</a>
            </div>`).join('');
    }

    async function loadData() {
        showState('加载中...');
        try {
            const resp = await fetch(`/api/portal/${encodeURIComponent(token)}/deliveries`);
            if (resp.status === 404) {
                els.companyName.textContent = '链接无效';
                els.metaText.textContent = '';
                showState('该链接无效或已失效，请联系发货方获取新链接');
                return;
            }
            if (!resp.ok) throw new Error('HTTP ' + resp.status);

            const data = await resp.json();
            allDeliveries = data.deliveries || [];

            els.companyName.textContent = `${data.logistics_name} — 待上传回单单据`;
            document.title = `${data.logistics_name} - 待上传回单单据`;

            if (!data.last_sync_at) {
                els.metaText.textContent = '数据同步中，请稍后刷新';
                showState('数据同步中，请稍后刷新');
                return;
            }

            els.metaText.textContent =
                `共 ${data.total} 张 · 数据更新于 ${formatTime(data.last_sync_at)}`;
            renderList();
        } catch (err) {
            showState('加载失败: ' + err.message + '，请稍后刷新重试');
        }
    }

    els.btnRefresh.addEventListener('click', loadData);
    els.searchInput.addEventListener('input', renderList);

    loadData();
})();
