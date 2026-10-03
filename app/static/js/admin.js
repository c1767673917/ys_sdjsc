const DEFAULT_ADMIN_UPLOAD_TYPE = '物流';

// 全局状态
const state = {
    currentPage: 1,
    pageSize: 20,
    totalPages: 1,
    totalRecords: 0,
    filters: {
        search: '',
        uploadType: DEFAULT_ADMIN_UPLOAD_TYPE,  // 默认显示物流数据
        docType: '',
        productType: '',
        logistics: '',  // 新增:物流筛选
        customerName: '',  // 新增:客户名称筛选(包含匹配)
        status: '',  // 新增：状态筛选
        startDate: '',
        endDate: ''
    },
    selectedIds: new Set(),  // 跟踪选中的记录ID
    exportPanelOpen: false,
    exportInProgress: false,
    exportOptions: {
        includeExcel: true,
        includeImages: true,
        includeDocNumber: true
    }
};

// 图片预览配置常量
const IMAGE_PREVIEW_CONFIG = {
    ZOOM_MIN: 0.1,              // 最小缩放比例 10%
    ZOOM_MAX: 5.0,              // 最大缩放比例 500%
    ZOOM_STEP_BUTTON: 0.2,      // 按钮缩放步长 20%
    ZOOM_STEP_WHEEL: 0.1,       // 滚轮缩放步长 10%
    WHEEL_DEBOUNCE_MS: 50,      // 滚轮防抖延迟 50ms
    IMAGE_PATH_PREFIX: '/uploaded_files/',  // 图片路径前缀
    LOAD_TIMEOUT_MS: 30000      // 图片加载超时 30秒
};

// 备注功能防抖定时器存储
const notesDebounceTimers = new Map();

// DOM元素
const elements = {
    // 统计
    statTotal: document.getElementById('statTotal'),
    statSuccess: document.getElementById('statSuccess'),
    statFailed: document.getElementById('statFailed'),
    statPending: document.getElementById('statPending'),  // 新增
    statUploading: document.getElementById('statUploading'),  // 新增

    // 筛选
    searchInput: document.getElementById('searchInput'),
    uploadTypeFilter: document.getElementById('uploadTypeFilter'),  // 新增:业务类型筛选
    docTypeFilter: document.getElementById('docTypeFilter'),
    productTypeFilter: document.getElementById('productTypeFilter'),
    statusFilter: document.getElementById('statusFilter'),  // 新增
    logisticsFilter: document.getElementById('logisticsFilter'),  // 新增:物流筛选
    customerNameFilter: document.getElementById('customerNameFilter'),  // 新增:客户名称筛选
    startDateInput: document.getElementById('startDateInput'),
    endDateInput: document.getElementById('endDateInput'),
    btnSearch: document.getElementById('btnSearch'),
    btnReset: document.getElementById('btnReset'),

    // 表格
    tableBody: document.getElementById('tableBody'),
    emptyState: document.getElementById('emptyState'),
    loadingState: document.getElementById('loadingState'),

    // 分页
    totalRecordsSpan: document.getElementById('totalRecords'),
    currentPageSpan: document.getElementById('currentPage'),
    totalPagesSpan: document.getElementById('totalPages'),
    btnFirstPage: document.getElementById('btnFirstPage'),
    btnPrevPage: document.getElementById('btnPrevPage'),
    btnNextPage: document.getElementById('btnNextPage'),
    btnLastPage: document.getElementById('btnLastPage'),

    // 操作
    btnRefresh: document.getElementById('btnRefresh'),
    btnExport: document.getElementById('btnExport'),
    exportOptionsPanel: document.getElementById('exportOptionsPanel'),
    exportExcelCheckbox: document.getElementById('exportExcelCheckbox'),
    exportImagesCheckbox: document.getElementById('exportImagesCheckbox'),
    exportDocNumberCheckbox: document.getElementById('exportDocNumberCheckbox'),
    btnConfirmExport: document.getElementById('btnConfirmExport'),
    exportArrow: document.querySelector('.export-arrow'),

    // 删除相关
    btnBatchDelete: document.getElementById('btnBatchDelete'),
    selectAll: document.getElementById('selectAll'),

    // Toast
    toast: document.getElementById('toast')
};

// 初始化
function init() {
    if (elements.uploadTypeFilter) {
        elements.uploadTypeFilter.value = DEFAULT_ADMIN_UPLOAD_TYPE;
    }

    // 绑定事件
    elements.btnSearch.addEventListener('click', handleSearch);
    elements.btnReset.addEventListener('click', handleReset);
    elements.btnRefresh.addEventListener('click', () => loadRecords());
    elements.btnExport.addEventListener('click', toggleExportPanel);
    elements.exportExcelCheckbox.addEventListener('change', updateConfirmButtonState);
    elements.exportImagesCheckbox.addEventListener('change', updateConfirmButtonState);
    if (elements.exportDocNumberCheckbox) {
        elements.exportDocNumberCheckbox.addEventListener('change', updateConfirmButtonState);
    }
    elements.btnConfirmExport.addEventListener('click', handleConfirmExport);

    elements.btnFirstPage.addEventListener('click', () => goToPage(1));
    elements.btnPrevPage.addEventListener('click', () => goToPage(state.currentPage - 1));
    elements.btnNextPage.addEventListener('click', () => goToPage(state.currentPage + 1));
    elements.btnLastPage.addEventListener('click', () => goToPage(state.totalPages));

    // 回车搜索
    elements.searchInput.addEventListener('keypress', (e) => {
        if (e.key === 'Enter') handleSearch();
    });

    // 客户名称输入框回车搜索
    if (elements.customerNameFilter) {
        elements.customerNameFilter.addEventListener('keypress', (e) => {
            if (e.key === 'Enter') handleSearch();
        });
    }

    // 删除相关事件
    elements.btnBatchDelete.addEventListener('click', handleBatchDelete);
    elements.selectAll.addEventListener('change', handleSelectAll);

    // 图片预览事件委托(使用事件委托,监听表格上的点击)
    elements.tableBody.addEventListener('click', handleFileNameClick);

    // 加载数据
    loadStatistics();
    loadLogisticsOptions();  // 新增:加载物流选项
    loadRecords();

    // 初始化WebDAV状态监控
    initWebDAVStatusMonitoring();

    // 更新迁移统计
    updateMigrationStats();

    // 初始化导出按钮状态
    updateConfirmButtonState();

    // 初始化折叠功能
    initCollapsibleCards();
}

// 加载统计数据
async function loadStatistics() {
    try {
        const response = await fetch('/api/admin/statistics');
        const data = await response.json();
        if (!response.ok) {
            throw new Error(getErrorDetail(data, response));
        }

        elements.statTotal.textContent = data.total_uploads;
        elements.statSuccess.textContent = data.success_count;
        elements.statFailed.textContent = data.failed_count;

        // 更新新增的统计项（如果元素存在）
        if (elements.statPending) {
            elements.statPending.textContent = data.pending_count || 0;
        }
        if (elements.statUploading) {
            elements.statUploading.textContent = data.uploading_count || 0;
        }

    } catch (error) {
        console.error('加载统计数据失败:', error);
        showToast('加载数据失败: ' + error.message, 'error');
    }
}

// 加载物流公司选项列表
async function loadLogisticsOptions() {
    try {
        const response = await fetch('/api/admin/logistics-options');
        if (!response.ok) {
            throw new Error('加载物流选项失败');
        }

        const data = await response.json();
        const selectElement = elements.logisticsFilter;

        // 清空现有选项 (保留"全部物流")
        selectElement.innerHTML = '<option value="全部物流">全部物流</option>';

        // 添加物流选项 (跳过第一个"全部物流",因为已添加)
        data.logistics_list.slice(1).forEach(logistics => {
            const option = document.createElement('option');
            option.value = logistics;
            option.textContent = logistics;
            selectElement.appendChild(option);
        });

        console.log('✓ 物流选项加载成功:', data.logistics_list.length - 1, '个选项');
    } catch (error) {
        console.error('✗ 加载物流选项失败:', error);
        // 降级处理: 保留"全部物流"选项
    }
}

// 加载记录列表
async function loadRecords() {
    // 显示加载状态
    elements.loadingState.style.display = 'block';
    elements.emptyState.style.display = 'none';
    elements.tableBody.innerHTML = '';

    try {
        // 构建查询参数
        const params = new URLSearchParams({
            page: state.currentPage,
            page_size: state.pageSize
        });

        if (state.filters.search) params.append('search', state.filters.search);
        if (state.filters.uploadType) params.append('upload_type', state.filters.uploadType);  // 新增:仅在选择具体业务类型时携带(全部业务时省略)
        if (state.filters.docType) params.append('doc_type', state.filters.docType);
        if (state.filters.productType) params.append('product_type', state.filters.productType);
        if (state.filters.status) params.append('status', state.filters.status);  // 新增
        if (state.filters.logistics && state.filters.logistics !== '全部物流') params.append('logistics', state.filters.logistics);  // 新增:物流筛选
        if (state.filters.customerName) params.append('customer_name', state.filters.customerName);  // 新增:客户名称包含匹配
        if (state.filters.startDate) params.append('start_date', state.filters.startDate);
        if (state.filters.endDate) params.append('end_date', state.filters.endDate);

        const response = await fetch(`/api/admin/records?${params}`);
        const data = await response.json();

        // 更新状态
        state.totalRecords = data.total;
        state.totalPages = data.total_pages;
        state.currentPage = data.page;

        // 隐藏加载状态
        elements.loadingState.style.display = 'none';

        // 显示数据或空状态
        if (data.records.length === 0) {
            elements.emptyState.style.display = 'block';
        } else {
            renderTable(data.records);
        }

        // 更新分页信息
        updatePagination();

    } catch (error) {
        elements.loadingState.style.display = 'none';
        showToast('加载数据失败: ' + error.message, 'error');
    }
}

// 渲染表格
function renderTable(records) {
    elements.tableBody.innerHTML = records.map(record => `
        <tr>
            <td>
                <input
                    type="checkbox"
                    class="row-checkbox"
                    data-id="${record.id}"
                    ${state.selectedIds.has(record.id) ? 'checked' : ''}
                >
            </td>
            <td>${escapeHtml(record.doc_number || '-')}</td>
            <td title="${escapeHtml(record.customer_name)}">${record.customer_name ? escapeHtml(record.customer_name) : '-'}</td>
            <td>${escapeHtml(record.doc_type || '-')}</td>
            <td>${escapeHtml(record.product_type || '')}</td>
            <td>${escapeHtml(formatDateTime(record.upload_time))}</td>
            <td>
                <span class="file-name file-name-clickable" data-record-id="${record.id}" data-filename="${escapeHtml(record.file_name)}" title="${escapeHtml(record.file_name)}">
                    ${escapeHtml(truncateFileName(record.file_name))}
                </span>
            </td>
            <td>${formatFileSize(record.file_size)}</td>
            <td>
                ${renderStatusBadge(record.status, record.error_message)}
            </td>
            <td>
                <button
                    class="btn-check ${record.checked ? 'checked' : 'unchecked'}"
                    data-id="${record.id}"
                    data-filename="${escapeHtml(record.file_name)}"
                    data-checked="${record.checked ? 'true' : 'false'}"
                >
                    ${record.checked ? '已检查' : '检查'}
                </button>
                <button class="btn-delete-row" data-id="${record.id}">删除</button>
            </td>
            <td>
                <input
                    type="text"
                    class="notes-input"
                    data-id="${record.id}"
                    value="${escapeHtml(record.notes || '')}"
                    maxlength="1000"
                    placeholder=""
                >
            </td>
        </tr>
    `).join('');

    // 绑定复选框事件
    document.querySelectorAll('.row-checkbox').forEach(checkbox => {
        checkbox.addEventListener('change', handleCheckboxChange);
    });

    // 绑定删除按钮事件
    document.querySelectorAll('.btn-delete-row').forEach(button => {
        button.addEventListener('click', (e) => {
            const recordId = parseInt(e.target.dataset.id);
            handleDeleteRow(recordId);
        });
    });

    // 绑定检查按钮事件
    document.querySelectorAll('.btn-check').forEach(button => {
        button.addEventListener('click', handleCheckButtonClick);
    });

    // 绑定备注输入框事件 (失焦自动保存)
    document.querySelectorAll('.notes-input').forEach(input => {
        input.addEventListener('blur', handleNotesBlur);
        input.addEventListener('input', handleNotesInput);
    });

    // 更新批量删除按钮状态
    updateBatchDeleteButton();
}

// 更新分页信息
function updatePagination() {
    // 无记录时 total_pages 为 0，仍显示第 1 页 / 共 1 页，翻页按钮全部禁用
    const empty = !(state.totalPages > 0);
    const displayPage = empty ? 1 : state.currentPage;
    const displayPages = empty ? 1 : state.totalPages;

    elements.totalRecordsSpan.textContent = state.totalRecords;
    elements.currentPageSpan.textContent = displayPage;
    elements.totalPagesSpan.textContent = displayPages;

    // 更新按钮状态
    elements.btnFirstPage.disabled = empty || state.currentPage === 1;
    elements.btnPrevPage.disabled = empty || state.currentPage === 1;
    elements.btnNextPage.disabled = empty || state.currentPage === state.totalPages;
    elements.btnLastPage.disabled = empty || state.currentPage === state.totalPages;
}

// 跳转页面
function goToPage(page) {
    if (page < 1 || page > state.totalPages) return;
    state.currentPage = page;
    loadRecords();
}

// 渲染状态徽章
function renderStatusBadge(status, errorMessage) {
    const statusConfig = {
        'success': { icon: '✓', text: '成功', class: 'status-success' },
        'uploading': { icon: '⏳', text: '上传中', class: 'status-uploading' },
        'failed': { icon: '✗', text: '失败', class: 'status-failed' },
        'pending': { icon: '⏸', text: '待上传', class: 'status-pending' }
    };

    const config = statusConfig[status] || statusConfig['pending'];
    let html = `<span class="status-badge ${config.class}">${config.icon} ${config.text}</span>`;

    // 失败状态显示错误信息（title 与正文都转义）
    if (status === 'failed' && errorMessage) {
        const safeError = escapeHtml(errorMessage);
        html += `<br><small style="color: #e74c3c;" title="${safeError}">${safeError}</small>`;
    }

    return html;
}

// 处理搜索
function handleSearch() {
    state.filters.search = elements.searchInput.value.trim();
    state.filters.uploadType = elements.uploadTypeFilter ? elements.uploadTypeFilter.value : '';  // 新增:业务类型筛选
    state.filters.docType = elements.docTypeFilter.value;
    state.filters.productType = elements.productTypeFilter.value;
    state.filters.status = elements.statusFilter ? elements.statusFilter.value : '';  // 新增
    state.filters.logistics = elements.logisticsFilter ? elements.logisticsFilter.value : '';  // 新增:物流筛选
    state.filters.customerName = elements.customerNameFilter ? elements.customerNameFilter.value.trim() : '';  // 新增:客户名称筛选
    state.filters.startDate = elements.startDateInput.value;
    state.filters.endDate = elements.endDateInput.value;

    state.currentPage = 1; // 重置到第一页
    loadRecords();
}

// 处理重置
function handleReset() {
    elements.searchInput.value = '';
    if (elements.uploadTypeFilter) {
        elements.uploadTypeFilter.value = DEFAULT_ADMIN_UPLOAD_TYPE;  // 重置后仍显示物流数据
    }
    elements.docTypeFilter.value = '';
    elements.productTypeFilter.value = '';
    if (elements.statusFilter) {
        elements.statusFilter.value = '';  // 新增
    }
    if (elements.logisticsFilter) {
        elements.logisticsFilter.value = '全部物流';  // 新增:重置物流筛选
    }
    if (elements.customerNameFilter) {
        elements.customerNameFilter.value = '';  // 新增:重置客户名称筛选
    }
    elements.startDateInput.value = '';
    elements.endDateInput.value = '';

    state.filters = {
        search: '',
        uploadType: DEFAULT_ADMIN_UPLOAD_TYPE,
        docType: '',
        productType: '',
        status: '',  // 新增
        logistics: '',  // 新增:物流筛选
        customerName: '',  // 新增:客户名称筛选
        startDate: '',
        endDate: ''
    };

    state.currentPage = 1;
    loadRecords();
}

// 切换导出面板
function toggleExportPanel() {
    state.exportPanelOpen = !state.exportPanelOpen;

    const panel = elements.exportOptionsPanel;
    const arrow = elements.exportArrow;
    const btn = elements.btnExport;

    if (state.exportPanelOpen) {
        panel.style.display = 'block';
        // 触发重绘以启用动画
        panel.offsetHeight; // eslint-disable-line no-unused-expressions
        panel.classList.add('open');
        arrow.classList.add('open');
        btn.setAttribute('aria-expanded', 'true');
    } else {
        panel.classList.remove('open');
        arrow.classList.remove('open');
        btn.setAttribute('aria-expanded', 'false');

        setTimeout(() => {
            if (!state.exportPanelOpen) {
                panel.style.display = 'none';
            }
        }, 250);
    }
}

// 关闭导出面板
function closeExportPanel() {
    if (state.exportPanelOpen) {
        toggleExportPanel();
    }
}

// 更新确认按钮状态
function updateConfirmButtonState() {
    const excelChecked = elements.exportExcelCheckbox.checked;
    const imagesChecked = elements.exportImagesCheckbox.checked;

    state.exportOptions.includeExcel = excelChecked;
    state.exportOptions.includeImages = imagesChecked;

    if (elements.exportDocNumberCheckbox) {
        elements.exportDocNumberCheckbox.disabled = !imagesChecked;
        state.exportOptions.includeDocNumber = elements.exportDocNumberCheckbox.checked;
    }

    const canExport = (excelChecked || imagesChecked) && !state.exportInProgress;
    elements.btnConfirmExport.disabled = !canExport;
}

// 生成带超时的AbortSignal（兼容旧浏览器）
function createTimeoutSignal(timeoutMs) {
    if (typeof AbortSignal !== 'undefined' && typeof AbortSignal.timeout === 'function') {
        return AbortSignal.timeout(timeoutMs);
    }

    const controller = new AbortController();
    setTimeout(() => controller.abort(), timeoutMs);
    return controller.signal;
}

// 处理确认导出
async function handleConfirmExport() {
    if (state.exportInProgress) return;

    const { includeExcel, includeImages, includeDocNumber } = state.exportOptions;

    if (!includeExcel && !includeImages) {
        showToast('请至少选择一项导出内容', 'warning');
        return;
    }

    state.exportInProgress = true;
    elements.btnConfirmExport.textContent = '导出中...';
    elements.btnConfirmExport.disabled = true;

    // 按要求，开始导出后立即关闭面板
    closeExportPanel();
    showToast('开始导出，请稍候...', 'success');

    try {
        const params = new URLSearchParams();

        if (state.filters.search) params.append('search', state.filters.search);
        if (state.filters.uploadType) params.append('upload_type', state.filters.uploadType);  // 新增:仅在选择具体业务类型时携带(全部业务时省略)
        if (state.filters.docType) params.append('doc_type', state.filters.docType);
        if (state.filters.productType) params.append('product_type', state.filters.productType);
        if (state.filters.status) params.append('status', state.filters.status);
        if (state.filters.logistics && state.filters.logistics !== '全部物流') params.append('logistics', state.filters.logistics);
        if (state.filters.customerName) params.append('customer_name', state.filters.customerName);
        if (state.filters.startDate) params.append('start_date', state.filters.startDate);
        if (state.filters.endDate) params.append('end_date', state.filters.endDate);

        params.append('include_excel', includeExcel.toString());
        params.append('include_images', includeImages.toString());
        params.append('include_doc_number', includeDocNumber.toString());

        // 使用新标签页下载，避免 blob 内存问题导致大文件卡住
        const url = `/api/admin/export?${params}`;
        const popup = window.open(url, '_blank');
        if (popup) {
            showToast('导出已在新标签页开始，请等待下载完成', 'success');
        } else {
            // 弹窗被拦截时改在当前页下载
            window.location.href = url;
            showToast('导出已开始', 'success');
        }
    } catch (error) {
        if (error.name === 'TimeoutError' || error.name === 'AbortError') {
            showToast('导出超时，请稍后重试', 'error');
        } else if (error.message && error.message.includes('NetworkError')) {
            showToast('导出失败: 网络连接错误', 'error');
        } else if (error.message && error.message.includes('Failed to fetch')) {
            showToast('导出失败: 网络连接错误', 'error');
        } else {
            showToast('导出失败: ' + error.message, 'error');
        }
    } finally {
        state.exportInProgress = false;
        elements.btnConfirmExport.textContent = '确认导出';
        updateConfirmButtonState();
    }
}

// 格式化日期时间（标准化为 YYYY-MM-DD HH:MM:SS 格式）
function formatDateTime(dateTimeStr) {
    if (!dateTimeStr) return '-';

    // 后端返回 ISO 8601 格式（如 '2025-10-15T14:30:45'）或标准格式
    // 直接解析字符串，不依赖浏览器时区，确保所有用户看到相同的北京时间
    const match = dateTimeStr.match(/^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2}):(\d{2})/);
    if (match) {
        return `${match[1]}-${match[2]}-${match[3]} ${match[4]}:${match[5]}:${match[6]}`;
    }

    // 降级处理：移除毫秒与时区偏移后直接返回
    const cleaned = dateTimeStr
        .replace('T', ' ')
        .replace(/\.\d+/, '')
        .replace(/([+-]\d{2}:?\d{2}|Z)$/i, '')
        .trim();

    if (/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(cleaned)) {
        return cleaned;
    }

    console.warn('未识别的时间格式，原样返回:', dateTimeStr);
    return dateTimeStr;
}

// 格式化文件大小
function formatFileSize(bytes) {
    if (!bytes) return '-';
    if (bytes < 1024) return bytes + ' B';
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
    return (bytes / 1024 / 1024).toFixed(1) + ' MB';
}

// 截断文件名显示（前17位+省略号）
function truncateFileName(fileName) {
    if (!fileName) return '-';
    if (fileName.length <= 17) return fileName;
    return fileName.substring(0, 17) + '...';
}

// 失败响应文案: detail 为字符串或 [{msg}]，否则用 HTTP 状态码文本
function getErrorDetail(data, response) {
    const detail = data && data.detail;
    if (typeof detail === 'string' && detail) {
        return detail;
    }
    if (Array.isArray(detail)) {
        const text = detail
            .map(item => (item && item.msg != null && item.msg !== '') ? String(item.msg) : '')
            .filter(Boolean)
            .join('; ');
        if (text) {
            return text;
        }
    }
    if (response && response.statusText) {
        return response.statusText;
    }
    return response ? ('HTTP ' + response.status) : '请求失败';
}

// HTML 转义: 客户名称等来自外部接口的文本拼入 innerHTML 前先转义, 防止 DOM 注入
function escapeHtml(value) {
    if (value === null || value === undefined) return '';
    return String(value)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

// 显示Toast
function showToast(message, type = 'success') {
    elements.toast.textContent = message;
    elements.toast.className = `toast ${type}`;
    elements.toast.style.display = 'block';

    setTimeout(() => {
        elements.toast.style.display = 'none';
    }, 3000);
}

// 处理全选/取消全选
function handleSelectAll() {
    const checkboxes = document.querySelectorAll('.row-checkbox');
    const isChecked = elements.selectAll.checked;

    checkboxes.forEach(checkbox => {
        checkbox.checked = isChecked;
        const recordId = parseInt(checkbox.dataset.id);

        if (isChecked) {
            state.selectedIds.add(recordId);
        } else {
            state.selectedIds.delete(recordId);
        }
    });

    updateBatchDeleteButton();
}

// 处理单行复选框变化
function handleCheckboxChange(event) {
    const recordId = parseInt(event.target.dataset.id);

    if (event.target.checked) {
        state.selectedIds.add(recordId);
    } else {
        state.selectedIds.delete(recordId);
    }

    // 更新全选框状态
    const checkboxes = document.querySelectorAll('.row-checkbox');
    const checkedCount = document.querySelectorAll('.row-checkbox:checked').length;
    elements.selectAll.checked = checkedCount === checkboxes.length;
    elements.selectAll.indeterminate = checkedCount > 0 && checkedCount < checkboxes.length;

    updateBatchDeleteButton();
}

// 更新批量删除按钮显示状态
function updateBatchDeleteButton() {
    if (state.selectedIds.size > 0) {
        elements.btnBatchDelete.style.display = 'inline-block';
        elements.btnBatchDelete.textContent = `批量删除 (${state.selectedIds.size})`;
    } else {
        elements.btnBatchDelete.style.display = 'none';
    }
}

// 处理批量删除
async function handleBatchDelete() {
    if (state.selectedIds.size === 0) {
        showToast('请至少选择一条记录', 'error');
        return;
    }

    const confirmMessage = `确定要删除选中的 ${state.selectedIds.size} 条记录吗？\n\n注意：这将标记记录为已删除，但不会删除本地文件。`;

    if (!confirm(confirmMessage)) {
        return;
    }

    try {
        const response = await fetch('/api/admin/records', {
            method: 'DELETE',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({
                ids: Array.from(state.selectedIds)
            })
        });

        if (!response.ok) {
            const error = await response.json();
            throw new Error(error.detail || '删除失败');
        }

        const result = await response.json();
        showToast(result.message, 'success');

        // 清空选中状态
        state.selectedIds.clear();
        elements.selectAll.checked = false;
        updateBatchDeleteButton();

        // 刷新列表和统计
        await Promise.all([loadRecords(), loadStatistics()]);

    } catch (error) {
        showToast('删除失败: ' + error.message, 'error');
    }
}

// 处理单行删除
async function handleDeleteRow(recordId) {
    const confirmMessage = '确定要删除这条记录吗？\n\n注意：这将标记记录为已删除，但不会删除本地文件。';

    if (!confirm(confirmMessage)) {
        return;
    }

    try {
        const response = await fetch('/api/admin/records', {
            method: 'DELETE',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({
                ids: [recordId]
            })
        });

        if (!response.ok) {
            const error = await response.json();
            throw new Error(error.detail || '删除失败');
        }

        const result = await response.json();
        showToast(result.message, 'success');

        // 从选中集合中移除（如果存在）
        state.selectedIds.delete(recordId);
        updateBatchDeleteButton();

        // 刷新列表和统计
        await Promise.all([loadRecords(), loadStatistics()]);

    } catch (error) {
        showToast('删除失败: ' + error.message, 'error');
    }
}

// ==================== 图片预览功能 ====================

// 图片预览状态
const imagePreviewState = {
    scale: 1,        // 缩放比例(1 = 100%)
    rotation: 0,     // 旋转角度(0, 90, 180, 270)
    minScale: IMAGE_PREVIEW_CONFIG.ZOOM_MIN,   // 最小缩放10%
    maxScale: IMAGE_PREVIEW_CONFIG.ZOOM_MAX    // 最大缩放500%
};

// 图片预览DOM元素
const imagePreviewElements = {
    modal: document.getElementById('imagePreviewModal'),
    overlay: null,  // 在显示时获取
    closeBtn: null,
    previewImage: document.getElementById('previewImage'),
    imageLoading: document.getElementById('imageLoading'),
    imageError: document.getElementById('imageError'),
    errorMessage: document.getElementById('errorMessage'),
    imageFileName: document.getElementById('imageFileName'),
    zoomLevel: document.getElementById('zoomLevel'),
    btnZoomIn: document.getElementById('btnZoomIn'),
    btnZoomOut: document.getElementById('btnZoomOut'),
    btnRotateLeft: document.getElementById('btnRotateLeft'),
    btnRotateRight: document.getElementById('btnRotateRight'),
    btnReset: document.getElementById('btnReset')
};

// 处理文件名点击事件(事件委托)
function handleFileNameClick(event) {
    // 检查是否点击了文件名元素
    const fileNameElement = event.target.closest('.file-name-clickable');
    if (!fileNameElement) return;

    const recordId = fileNameElement.dataset.recordId;
    const filename = fileNameElement.dataset.filename;
    if (recordId && filename) {
        openImagePreview(recordId, filename);
    }
}

// 打开图片预览模态框
function openImagePreview(recordId, filename) {
    // 重置状态
    imagePreviewState.scale = 1;
    imagePreviewState.rotation = 0;

    // 显示模态框
    imagePreviewElements.modal.style.display = 'flex';

    // 显示加载状态
    imagePreviewElements.imageLoading.style.display = 'block';
    imagePreviewElements.previewImage.style.display = 'none';
    imagePreviewElements.imageError.style.display = 'none';

    // 设置文件名
    imagePreviewElements.imageFileName.textContent = filename;

    // 更新缩放显示
    updateZoomDisplay();

    // 使用API端点通过record_id获取图片
    const imageUrl = `/api/admin/files/${recordId}/preview`;
    const img = new Image();

    // 添加加载超时处理
    let loadTimeout = setTimeout(() => {
        img.src = '';  // 取消加载
        imagePreviewElements.imageLoading.style.display = 'none';
        imagePreviewElements.imageError.style.display = 'block';
        imagePreviewElements.errorMessage.textContent = '图片加载超时，请检查网络';
        enableToolbarButtons(false);
    }, IMAGE_PREVIEW_CONFIG.LOAD_TIMEOUT_MS);

    img.onload = function() {
        clearTimeout(loadTimeout);

        // 隐藏加载状态
        imagePreviewElements.imageLoading.style.display = 'none';

        // 显示图片
        imagePreviewElements.previewImage.src = imageUrl;
        imagePreviewElements.previewImage.style.display = 'block';

        // 显示图片尺寸信息
        const sizeInfo = `${this.naturalWidth} × ${this.naturalHeight}`;
        imagePreviewElements.imageFileName.textContent = `${filename} (${sizeInfo})`;

        // 启用工具栏按钮
        enableToolbarButtons(true);

        // 应用初始变换
        applyImageTransform();
    };

    img.onerror = function() {
        clearTimeout(loadTimeout);

        // 隐藏加载状态
        imagePreviewElements.imageLoading.style.display = 'none';

        // 显示错误提示
        imagePreviewElements.imageError.style.display = 'block';
        imagePreviewElements.errorMessage.textContent = '文件不存在或无法访问';

        // 禁用工具栏按钮
        enableToolbarButtons(false);
    };

    img.src = imageUrl;

    // 绑定事件(仅在首次打开时绑定)
    if (!imagePreviewElements.overlay) {
        imagePreviewElements.overlay = imagePreviewElements.modal.querySelector('.image-modal-overlay');
        imagePreviewElements.closeBtn = imagePreviewElements.modal.querySelector('.image-modal-close');

        // 点击遮罩层关闭
        imagePreviewElements.overlay.addEventListener('click', closeImagePreview);

        // 点击关闭按钮
        imagePreviewElements.closeBtn.addEventListener('click', closeImagePreview);

        // ESC键关闭
        document.addEventListener('keydown', function(e) {
            if (e.key === 'Escape' && imagePreviewElements.modal.style.display === 'flex') {
                closeImagePreview();
            }
        });

        // 工具栏按钮事件
        imagePreviewElements.btnZoomIn.addEventListener('click', () => zoomImage(IMAGE_PREVIEW_CONFIG.ZOOM_STEP_BUTTON));
        imagePreviewElements.btnZoomOut.addEventListener('click', () => zoomImage(-IMAGE_PREVIEW_CONFIG.ZOOM_STEP_BUTTON));
        imagePreviewElements.btnRotateLeft.addEventListener('click', () => rotateImage(-90));
        imagePreviewElements.btnRotateRight.addEventListener('click', () => rotateImage(90));
        imagePreviewElements.btnReset.addEventListener('click', resetImageTransform);

        // 鼠标滚轮缩放
        imagePreviewElements.previewImage.addEventListener('wheel', handleMouseWheel, { passive: false });
    }
}

// 关闭图片预览
function closeImagePreview() {
    imagePreviewElements.modal.style.display = 'none';
    imagePreviewElements.previewImage.src = '';
}

// 缩放图片
function zoomImage(delta) {
    const newScale = imagePreviewState.scale + delta;

    // 限制范围
    if (newScale < imagePreviewState.minScale || newScale > imagePreviewState.maxScale) {
        return;
    }

    imagePreviewState.scale = newScale;
    applyImageTransform();
    updateZoomDisplay();
}

// 旋转图片
function rotateImage(degrees) {
    imagePreviewState.rotation = (imagePreviewState.rotation + degrees) % 360;

    // 处理负数角度
    if (imagePreviewState.rotation < 0) {
        imagePreviewState.rotation += 360;
    }

    applyImageTransform();
}

// 重置图片变换
function resetImageTransform() {
    imagePreviewState.scale = 1;
    imagePreviewState.rotation = 0;
    applyImageTransform();
    updateZoomDisplay();
}

// 应用图片变换(CSS transform)
function applyImageTransform() {
    const transform = `scale(${imagePreviewState.scale}) rotate(${imagePreviewState.rotation}deg)`;
    imagePreviewElements.previewImage.style.transform = transform;
}

// 更新缩放显示
function updateZoomDisplay() {
    const percentage = Math.round(imagePreviewState.scale * 100);
    imagePreviewElements.zoomLevel.textContent = `${percentage}%`;

    // 更新按钮状态
    imagePreviewElements.btnZoomOut.disabled = imagePreviewState.scale <= imagePreviewState.minScale;
    imagePreviewElements.btnZoomIn.disabled = imagePreviewState.scale >= imagePreviewState.maxScale;
}

// 启用/禁用工具栏按钮
function enableToolbarButtons(enabled) {
    imagePreviewElements.btnZoomIn.disabled = !enabled;
    imagePreviewElements.btnZoomOut.disabled = !enabled;
    imagePreviewElements.btnRotateLeft.disabled = !enabled;
    imagePreviewElements.btnRotateRight.disabled = !enabled;
    imagePreviewElements.btnReset.disabled = !enabled;
}

// 处理鼠标滚轮缩放(带防抖)
let wheelTimeout = null;
function handleMouseWheel(event) {
    event.preventDefault();

    // 防抖处理
    if (wheelTimeout) {
        clearTimeout(wheelTimeout);
    }

    wheelTimeout = setTimeout(() => {
        // 向上滚动放大,向下滚动缩小
        const delta = event.deltaY > 0 ? -IMAGE_PREVIEW_CONFIG.ZOOM_STEP_WHEEL : IMAGE_PREVIEW_CONFIG.ZOOM_STEP_WHEEL;
        zoomImage(delta);
    }, IMAGE_PREVIEW_CONFIG.WHEEL_DEBOUNCE_MS);
}

// ==================== 检查按钮功能 ====================

/**
 * 处理检查按钮点击事件
 * 逻辑:
 * - 未检查状态: 打开图片模态框 → 关闭后自动标记为已检查
 * - 已检查状态: 弹出确认对话框 → 确认后改回未检查
 */
async function handleCheckButtonClick(event) {
    const button = event.target;
    const recordId = parseInt(button.dataset.id);
    const filename = button.dataset.filename;
    const isChecked = button.dataset.checked === 'true';

    if (!isChecked) {
        // 流程1: 未检查 → 打开图片 → 自动标记已检查
        handleCheckImage(recordId, filename, button);
    } else {
        // 流程2: 已检查 → 确认撤销 → 改回未检查
        handleUncheckConfirm(recordId, button);
    }
}

/**
 * 处理"检查"按钮点击 - 显示图片并自动标记已检查
 */
function handleCheckImage(recordId, filename, buttonElement) {
    // 1. 打开图片模态框(复用现有函数)
    openImagePreview(recordId, filename);

    // 2. 监听模态框关闭事件
    const modal = imagePreviewElements.modal;
    const originalDisplay = modal.style.display;

    // 轮询检测模态框关闭(每500ms检查一次)
    const checkInterval = setInterval(async () => {
        if (modal.style.display === 'none' && originalDisplay === 'flex') {
            // 模态框已关闭
            clearInterval(checkInterval);

            // 3. 调用API更新状态为已检查
            const success = await updateCheckStatus(recordId, true);

            if (success) {
                // 4. 更新按钮UI
                updateCheckButtonUI(buttonElement, true);
                showToast('已标记为已检查', 'success');
            }
        }
    }, 500);

    // 安全机制: 60秒后自动停止轮询(防止内存泄漏)
    setTimeout(() => {
        clearInterval(checkInterval);
    }, 60000);
}

/**
 * 处理"已检查"按钮点击 - 确认撤销检查状态
 */
async function handleUncheckConfirm(recordId, buttonElement) {
    // 1. 显示确认对话框
    const confirmed = confirm('确认改回检查状态吗?\n\n撤销后需要重新检查图片。');

    if (!confirmed) {
        return;  // 用户取消
    }

    // 2. 调用API更新状态为未检查
    const success = await updateCheckStatus(recordId, false);

    if (success) {
        // 3. 更新按钮UI
        updateCheckButtonUI(buttonElement, false);
        showToast('已改回检查状态', 'success');
    }
}

/**
 * 调用后端API更新检查状态
 * @param {number} recordId - 记录ID
 * @param {boolean} checked - 检查状态
 * @returns {Promise<boolean>} 是否成功
 */
async function updateCheckStatus(recordId, checked) {
    try {
        const response = await fetch(`/api/admin/records/${recordId}/check`, {
            method: 'PATCH',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({ checked })
        });

        if (!response.ok) {
            const error = await response.json();
            throw new Error(error.detail || '更新失败');
        }

        return true;

    } catch (error) {
        showToast('更新检查状态失败: ' + error.message, 'error');
        return false;
    }
}

/**
 * 更新检查按钮的UI状态
 * @param {HTMLElement} buttonElement - 按钮DOM元素
 * @param {boolean} checked - 新的检查状态
 */
function updateCheckButtonUI(buttonElement, checked) {
    if (checked) {
        // 已检查状态
        buttonElement.textContent = '已检查';
        buttonElement.classList.remove('unchecked');
        buttonElement.classList.add('checked');
        buttonElement.dataset.checked = 'true';
    } else {
        // 未检查状态
        buttonElement.textContent = '检查';
        buttonElement.classList.remove('checked');
        buttonElement.classList.add('unchecked');
        buttonElement.dataset.checked = 'false';
    }
}

// ==================== 备注功能 ====================

/**
 * 处理备注输入框失焦事件 - 自动保存（带防抖）
 */
async function handleNotesBlur(event) {
    const input = event.target;
    const recordId = parseInt(input.dataset.id);
    const notes = input.value.trim();

    // 清除已有的防抖定时器
    if (notesDebounceTimers.has(recordId)) {
        clearTimeout(notesDebounceTimers.get(recordId));
    }

    // 设置300ms防抖延迟
    const timer = setTimeout(async () => {
        // 移除错误状态
        input.classList.remove('error');

        try {
            const response = await fetch(`/api/admin/records/${recordId}/notes`, {
                method: 'PATCH',
                headers: {
                    'Content-Type': 'application/json'
                },
                body: JSON.stringify({ notes })
            });

            if (!response.ok) {
                const error = await response.json();
                throw new Error(error.detail || '保存失败');
            }

            // 保存成功 - 可选：显示成功提示
            // showToast('备注已保存', 'success');

        } catch (error) {
            // 保存失败 - 显示错误状态
            input.classList.add('error');
            showToast('保存备注失败: ' + error.message, 'error');
        } finally {
            // 清理定时器
            notesDebounceTimers.delete(recordId);
        }
    }, 300);

    // 存储定时器
    notesDebounceTimers.set(recordId, timer);
}

/**
 * 处理备注输入框输入事件 - 移除错误状态
 */
function handleNotesInput(event) {
    const input = event.target;
    input.classList.remove('error');
}

// WebDAV和迁移功能
let webdavStatusInterval = null;
let currentMigrationId = null;
let migrationStatusInterval = null;

/**
 * 初始化WebDAV状态监控
 */
function initWebDAVStatusMonitoring() {
    // 立即检查一次
    checkWebDAVStatus();

    // 每30秒检查一次WebDAV状态
    webdavStatusInterval = setInterval(checkWebDAVStatus, 30000);

    // 绑定事件
    document.getElementById('btnSyncNow').addEventListener('click', syncNow);
    document.getElementById('btnCheckConnection').addEventListener('click', checkConnection);
    document.getElementById('btnStartMigration').addEventListener('click', startMigration);
    document.getElementById('btnDryRunMigration').addEventListener('click', startDryRunMigration);
    document.getElementById('btnCloseMigrationStatus').addEventListener('click', closeMigrationStatus);
}

/**
 * 检查WebDAV状态
 */
async function checkWebDAVStatus() {
    try {
        const response = await fetch('/api/admin/webdav/status');
        const data = await response.json();

        if (data.success) {
            updateWebDAVStatusDisplay(data);
        }
    } catch (error) {
        console.error('检查WebDAV状态失败:', error);
        updateWebDAVStatusDisplay({
            webdav_available: false,
            pending_sync_count: 0,
            total_cached_files: 0,
            cache_size_mb: 0
        });
    }
}

/**
 * 更新WebDAV状态显示
 */
function updateWebDAVStatusDisplay(data) {
    const statusDot = document.getElementById('webdavStatusDot');
    const statusText = document.getElementById('webdavStatusText');
    const pendingCount = document.getElementById('pendingSyncCount');
    const cachedCount = document.getElementById('cachedFilesCount');
    const cacheSize = document.getElementById('cacheSize');

    // 更新状态指示器
    statusDot.className = 'status-dot';
    if (data.webdav_available) {
        statusDot.classList.add('online');
        statusText.textContent = 'WebDAV服务正常';
    } else {
        statusDot.classList.add('offline');
        statusText.textContent = 'WebDAV服务不可用';
    }

    // 更新详细信息
    pendingCount.textContent = data.pending_sync_count || 0;
    cachedCount.textContent = data.total_cached_files || 0;
    cacheSize.textContent = `${(data.cache_size_mb || 0).toFixed(2)} MB`;
}

/**
 * 立即同步
 */
async function syncNow() {
    const btn = document.getElementById('btnSyncNow');
    btn.disabled = true;
    btn.textContent = '同步中...';

    try {
        const response = await fetch('/api/admin/webdav/sync', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({ force: false })
        });

        const data = await response.json();

        if (!response.ok) {
            showToast(getErrorDetail(data, response), 'error');
        } else if (data.success) {
            showToast(data.message || '同步任务已启动', 'success');
            // 3秒后刷新状态
            setTimeout(checkWebDAVStatus, 3000);
        } else {
            showToast('同步失败', 'error');
        }
    } catch (error) {
        console.error('同步失败:', error);
        showToast('同步失败: ' + error.message, 'error');
    } finally {
        btn.disabled = false;
        btn.textContent = '立即同步';
    }
}

/**
 * 检查WebDAV连接
 */
async function checkConnection() {
    const btn = document.getElementById('btnCheckConnection');
    btn.disabled = true;
    btn.textContent = '检查中...';

    try {
        const response = await fetch('/api/admin/webdav/test-connection', {
            method: 'POST'
        });

        const data = await response.json();

        if (data.success) {
            const tests = data.test_results;
            let message = '连接测试结果:\n';
            message += `• 连接状态: ${tests.connection ? '✅' : '❌'}\n`;
            message += `• 身份验证: ${tests.authentication ? '✅' : '❌'}\n`;
            message += `• 写入权限: ${tests.write_permission ? '✅' : '❌'}\n`;
            message += `• 读取权限: ${tests.read_permission ? '✅' : '❌'}`;

            showToast(message, tests.connection ? 'success' : 'error');
        } else {
            showToast(data.message || '连接测试失败', 'error');
        }
    } catch (error) {
        console.error('连接测试失败:', error);
        showToast('连接测试失败: ' + error.message, 'error');
    } finally {
        btn.disabled = false;
        btn.textContent = '检查连接';
    }
}

/**
 * 更新迁移统计信息
 */
async function updateMigrationStats() {
    try {
        const response = await fetch('/api/admin/migration/stats');
        const data = await response.json();

        if (data.success) {
            const localCount = document.getElementById('localFilesCount');
            const migratedCount = document.getElementById('migratedFilesCount');
            const progressFill = document.getElementById('migrationProgressFill');
            const progressText = document.getElementById('migrationProgressText');

            localCount.textContent = data.local_files.count;
            migratedCount.textContent = data.migrated_files.count;

            const progress = data.migration_progress || 0;
            progressFill.style.width = `${progress}%`;
            progressText.textContent = `${progress.toFixed(1)}%`;
        }
    } catch (error) {
        console.error('获取迁移统计失败:', error);
    }
}

/**
 * 开始迁移
 */
async function startMigration() {
    if (currentMigrationId) {
        showToast('已有迁移任务在进行中', 'warning');
        return;
    }

    if (!confirm('确定要开始迁移所有本地文件到WebDAV吗？这可能需要一些时间。')) {
        return;
    }

    try {
        const response = await fetch('/api/admin/migration/start', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({ dry_run: false })
        });

        const data = await response.json();

        if (!response.ok) {
            showToast(getErrorDetail(data, response), 'error');
        } else if (data.success) {
            currentMigrationId = data.migration_id;
            showMigrationStatus();
            startMigrationStatusMonitoring();
            showToast(`迁移任务已启动，共${data.total_files}个文件`, 'success');
        } else {
            showToast('启动迁移失败', 'error');
        }
    } catch (error) {
        console.error('启动迁移失败:', error);
        showToast('启动迁移失败: ' + error.message, 'error');
    }
}

/**
 * 演练模式迁移
 */
async function startDryRunMigration() {
    if (currentMigrationId) {
        showToast('已有迁移任务在进行中', 'warning');
        return;
    }

    try {
        const response = await fetch('/api/admin/migration/start', {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json'
            },
            body: JSON.stringify({ dry_run: true })
        });

        const data = await response.json();

        if (!response.ok) {
            showToast(getErrorDetail(data, response), 'error');
        } else if (data.success) {
            currentMigrationId = data.migration_id;
            showMigrationStatus();
            startMigrationStatusMonitoring();
            showToast(`演练模式已启动，共${data.total_files}个文件`, 'info');
        } else {
            showToast('启动演练失败', 'error');
        }
    } catch (error) {
        console.error('启动演练失败:', error);
        showToast('启动演练失败: ' + error.message, 'error');
    }
}

/**
 * 显示迁移状态
 */
function showMigrationStatus() {
    const statusDiv = document.getElementById('migrationStatus');
    statusDiv.style.display = 'block';
}

/**
 * 关闭迁移状态
 */
function closeMigrationStatus() {
    const statusDiv = document.getElementById('migrationStatus');
    statusDiv.style.display = 'none';

    if (migrationStatusInterval) {
        clearInterval(migrationStatusInterval);
        migrationStatusInterval = null;
    }

    currentMigrationId = null;
}

/**
 * 开始监控迁移状态
 */
function startMigrationStatusMonitoring() {
    if (migrationStatusInterval) {
        clearInterval(migrationStatusInterval);
    }

    migrationStatusInterval = setInterval(() => {
        checkMigrationStatus();
    }, 2000); // 每2秒检查一次
}

/**
 * 检查迁移状态
 */
async function checkMigrationStatus() {
    if (!currentMigrationId) return;

    try {
        const response = await fetch(`/api/admin/migration/status/${currentMigrationId}`);
        const data = await response.json();

        if (data.success) {
            updateMigrationStatusDisplay(data);

            // 如果迁移完成，停止监控
            if (data.status === 'completed' || data.status === 'completed_with_errors' || data.status === 'failed') {
                if (migrationStatusInterval) {
                    clearInterval(migrationStatusInterval);
                    migrationStatusInterval = null;
                }

                // 3秒后自动关闭状态显示
                setTimeout(() => {
                    closeMigrationStatus();
                    // 刷新迁移统计
                    updateMigrationStats();
                    // 刷新WebDAV状态
                    checkWebDAVStatus();
                }, 3000);
            }
        }
    } catch (error) {
        console.error('检查迁移状态失败:', error);
    }
}

/**
 * 更新迁移状态显示
 */
function updateMigrationStatusDisplay(data) {
    const statusText = document.getElementById('migrationStatusText');
    const totalSpan = document.getElementById('migrationTotal');
    const completedSpan = document.getElementById('migrationCompleted');
    const failedSpan = document.getElementById('migrationFailed');
    const progressBar = document.getElementById('migrationProgressBar');
    const errorsDiv = document.getElementById('migrationErrors');

    // 更新状态文本
    const statusMap = {
        'pending': '准备中...',
        'running': '迁移中...',
        'completed': '迁移完成',
        'completed_with_errors': '迁移完成（部分失败）',
        'failed': '迁移失败'
    };

    statusText.textContent = statusMap[data.status] || data.status;

    // 更新进度
    if (data.progress) {
        totalSpan.textContent = data.progress.total;
        completedSpan.textContent = data.progress.completed;
        failedSpan.textContent = data.progress.failed;
        progressBar.style.width = `${data.progress.percentage}%`;
    }

    // 显示错误信息
    if (data.errors && data.errors.length > 0) {
        errorsDiv.innerHTML = data.errors.map(error =>
            `<div class="error-item">
                <strong>${escapeHtml(error.filename)}:</strong> ${escapeHtml(error.error)}
            </div>`
        ).join('');
    } else {
        errorsDiv.innerHTML = '';
    }
}

/**
 * 显示提示消息
 */
function showToast(message, type = 'info') {
    // 创建提示元素
    const toast = document.createElement('div');
    toast.className = `toast toast-${type}`;
    toast.textContent = message;

    // 添加样式
    Object.assign(toast.style, {
        position: 'fixed',
        top: '20px',
        right: '20px',
        padding: '12px 20px',
        borderRadius: '6px',
        color: 'white',
        fontWeight: '500',
        zIndex: '10000',
        opacity: '0',
        transform: 'translateY(-20px)',
        transition: 'all 0.3s ease'
    });

    // 设置背景色
    const colors = {
        success: '#27ae60',
        error: '#e74c3c',
        warning: '#f39c12',
        info: '#3498db'
    };
    toast.style.backgroundColor = colors[type] || colors.info;

    document.body.appendChild(toast);

    // 显示动画
    setTimeout(() => {
        toast.style.opacity = '1';
        toast.style.transform = 'translateY(0)';
    }, 10);

    // 3秒后移除
    setTimeout(() => {
        toast.style.opacity = '0';
        toast.style.transform = 'translateY(-20px)';
        setTimeout(() => {
            if (toast.parentNode) {
                toast.parentNode.removeChild(toast);
            }
        }, 300);
    }, 3000);
}

// ==================== 折叠功能 ====================

/**
 * 初始化折叠卡片功能
 */
function initCollapsibleCards() {
    // WebDAV状态卡片折叠
    const webdavHeader = document.getElementById('webdavCardHeader');
    const webdavContent = document.getElementById('webdavCollapsibleContent');

    if (webdavHeader && webdavContent) {
        webdavHeader.addEventListener('click', () => {
            toggleCollapsible(webdavContent, webdavHeader.querySelector('.collapse-icon'));
        });
    }

    // 文件迁移卡片折叠
    const migrationHeader = document.getElementById('migrationCardHeader');
    const migrationContent = document.getElementById('migrationCollapsibleContent');

    if (migrationHeader && migrationContent) {
        migrationHeader.addEventListener('click', () => {
            toggleCollapsible(migrationContent, migrationHeader.querySelector('.collapse-icon'));
        });
    }
}

/**
 * 切换折叠状态
 * @param {HTMLElement} content - 可折叠内容元素
 * @param {HTMLElement} icon - 折叠图标元素
 */
function toggleCollapsible(content, icon) {
    if (content.style.display === 'none' || content.style.display === '') {
        // 展开
        content.style.display = 'block';
        if (icon) {
            icon.textContent = '▲';
        }
    } else {
        // 折叠
        content.style.display = 'none';
        if (icon) {
            icon.textContent = '▼';
        }
    }
}

// 启动应用
init();
