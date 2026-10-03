// 全局状态
const state = {
    businessId: '',
    docNumber: '',
    docType: '',
    productType: '',  // 产品类型(如:油脂/快消)
    uploadType: '',  // 上传业务类型: 物流/仓库 (默认未选择)
    selectedFiles: [],
    maxFiles: 10,
    maxFileSize: 10 * 1024 * 1024, // 10MB
    uploading: false
};

// DOM元素
const elements = {
    businessIdDisplay: document.getElementById('businessIdDisplay'),
    uploadTypeDisplay: document.getElementById('uploadTypeDisplay'),
    uploadTypeValue: document.getElementById('uploadTypeValue'),
    uploadTypeSection: document.getElementById('uploadTypeSection'),
    btnTypeLogistics: document.getElementById('btnTypeLogistics'),
    btnTypeWarehouse: document.getElementById('btnTypeWarehouse'),
    uploadArea: document.getElementById('uploadArea'),
    fileInput: document.getElementById('fileInput'),
    previewSection: document.getElementById('previewSection'),
    previewList: document.getElementById('previewList'),
    selectedCount: document.getElementById('selectedCount'),
    btnClear: document.getElementById('btnClear'),
    btnUpload: document.getElementById('btnUpload'),
    progressSection: document.getElementById('progressSection'),
    progressBar: document.getElementById('progressBar'),
    progressText: document.getElementById('progressText'),
    progressList: document.getElementById('progressList'),
    toast: document.getElementById('toast'),
    btnHistory: document.getElementById('btnHistory'),
    historyModal: document.getElementById('historyModal'),
    historyList: document.getElementById('historyList'),
    btnCloseModal: document.getElementById('btnCloseModal')
};

// 初始化
function init() {
    // 从URL查询参数提取参数
    const urlParams = new URLSearchParams(window.location.search);
    state.businessId = urlParams.get('business_id');
    state.docNumber = urlParams.get('doc_number');
    state.docType = urlParams.get('doc_type');
    state.productType = urlParams.get('product_type') || '';  // 产品类型(可选参数)

    // 验证必填参数
    if (!state.businessId || !/^\d+$/.test(state.businessId)) {
        showToast('错误的业务单据ID，请扫描正确的二维码', 'error');
        return;
    }

    if (!state.docNumber || state.docNumber.trim().length === 0) {
        showToast('缺少单据编号参数', 'error');
        return;
    }

    if (!state.docType || !['销售', '转库', '其他'].includes(state.docType)) {
        showToast('单据类型参数错误', 'error');
        return;
    }

    // 显示参数信息
    elements.businessIdDisplay.textContent = `${state.docType} - ${state.docNumber}`;

    // 根据单据类型设置主题色
    setThemeByDocType(state.docType);

    // 绑定事件
    elements.btnTypeLogistics.addEventListener('click', () => selectUploadType('物流'));
    elements.btnTypeWarehouse.addEventListener('click', () => selectUploadType('仓库'));
    elements.uploadArea.addEventListener('click', () => elements.fileInput.click());
    elements.fileInput.addEventListener('change', handleFileSelect);
    elements.btnClear.addEventListener('click', clearFiles);
    elements.btnUpload.addEventListener('click', uploadFiles);
    elements.btnHistory.addEventListener('click', showHistory);
    elements.btnCloseModal.addEventListener('click', () => elements.historyModal.style.display = 'none');

    // 初始锁定上传控件,直到用户选择业务类型
    updateUploadControlsAvailability();
}

// 选择上传业务类型(物流/仓库)
function selectUploadType(uploadType) {
    // 上传进行中时禁止切换
    if (state.uploading) {
        return;
    }

    // 切换类型时清空已选文件和验证状态,避免跨类型误提交
    if (state.uploadType && state.uploadType !== uploadType) {
        clearFiles();
    }

    state.uploadType = uploadType;

    // 高亮选中按钮
    elements.btnTypeLogistics.classList.toggle('active', uploadType === '物流');
    elements.btnTypeWarehouse.classList.toggle('active', uploadType === '仓库');

    // 在单据信息旁显示已选业务类型
    elements.uploadTypeValue.textContent = uploadType;
    elements.uploadTypeDisplay.style.display = 'block';

    updateUploadControlsAvailability();
}

// 根据是否已选择业务类型启用/禁用上传控件
function updateUploadControlsAvailability() {
    const enabled = !!state.uploadType;

    // 选择类型前,锁定文件选择/预览/上传控件
    elements.uploadArea.classList.toggle('disabled', !enabled);
    elements.fileInput.disabled = !enabled;
    elements.btnUpload.disabled = !enabled || state.selectedFiles.length === 0;
}

// 根据单据类型设置主题
function setThemeByDocType(docType) {
    const header = document.querySelector('.header');

    // 移除现有主题类
    header.classList.remove('theme-sales', 'theme-transfer', 'theme-other');

    // 根据类型添加主题类
    if (docType === '销售') {
        header.classList.add('theme-sales');
    } else if (docType === '转库') {
        header.classList.add('theme-transfer');
    } else {
        header.classList.add('theme-other');
    }
}

// 文件选择处理
async function handleFileSelect(e) {
    // 必须先选择业务类型
    if (!state.uploadType) {
        showToast('请先选择业务类型(物流/仓库)', 'error');
        e.target.value = '';
        return;
    }

    const files = Array.from(e.target.files);

    // 验证文件数量
    if (state.selectedFiles.length + files.length > state.maxFiles) {
        showToast(`最多只能选择${state.maxFiles}张图片`, 'error');
        return;
    }

    // 验证文件
    const validFiles = [];
    for (const file of files) {
        // 检查文件类型
        if (!file.type.startsWith('image/')) {
            showToast(`${file.name} 不是图片文件`, 'error');
            continue;
        }

        // 检查文件大小
        if (file.size > state.maxFileSize) {
            showToast(`${file.name} 超过10MB限制`, 'error');
            continue;
        }

        validFiles.push(file);
    }

    // 重置input
    e.target.value = '';

    if (validFiles.length === 0) {
        return;
    }

    state.selectedFiles.push(...validFiles);
    // 更新预览
    updatePreview();
}

// 更新预览
function updatePreview() {
    if (state.selectedFiles.length === 0) {
        elements.previewSection.style.display = 'none';
        elements.btnUpload.disabled = true;
        return;
    }

    elements.previewSection.style.display = 'block';
    // 仅当已选择业务类型时才允许上传
    elements.btnUpload.disabled = !state.uploadType;
    elements.selectedCount.textContent = state.selectedFiles.length;

    // 清空预览列表
    elements.previewList.innerHTML = '';

    // 生成预览
    state.selectedFiles.forEach((file, index) => {
        const reader = new FileReader();
        reader.onload = (e) => {
            const item = document.createElement('div');
            item.className = 'preview-item';

            item.innerHTML = `
                <img src="${e.target.result}" alt="${file.name}">
                <button class="btn-remove" onclick="removeFile(${index})">×</button>
            `;
            elements.previewList.appendChild(item);
        };
        reader.readAsDataURL(file);
    });
}

// 移除文件
function removeFile(index) {
    state.selectedFiles.splice(index, 1);
    updatePreview();
}

// 清空文件
function clearFiles() {
    state.selectedFiles = [];
    updatePreview();
}

// 上传文件
async function uploadFiles() {
    if (state.uploading || state.selectedFiles.length === 0) {
        return;
    }

    state.uploading = true;
    elements.btnUpload.disabled = true;
    elements.progressSection.style.display = 'block';
    elements.progressList.innerHTML = '';

    // 准备FormData
    const formData = new FormData();
    formData.append('business_id', state.businessId);
    formData.append('doc_number', state.docNumber);
    formData.append('doc_type', state.docType);
    formData.append('upload_type', state.uploadType);  // 上传业务类型: 物流/仓库
    if (state.productType) {
        formData.append('product_type', state.productType);  // 添加产品类型参数(如果存在)
    }
    state.selectedFiles.forEach(file => {
        formData.append('files', file);
    });

    // 创建进度项
    state.selectedFiles.forEach(file => {
        const item = document.createElement('div');
        item.className = 'progress-item';
        item.innerHTML = `
            <div class="filename">${file.name}</div>
            <div class="status loading">⏳</div>
        `;
        elements.progressList.appendChild(item);
    });

    try {
        // 发送请求
        const response = await fetch('/api/upload', {
            method: 'POST',
            body: formData
        });

        const result = await response.json();

        if (!response.ok) {
            throw new Error(result.detail || '上传失败');
        }

        // 异步上传：所有文件都已提交到后台处理
        const progressItems = elements.progressList.querySelectorAll('.progress-item');
        result.records.forEach((record, index) => {
            const statusEl = progressItems[index].querySelector('.status');

            // 标记为已提交（后台处理中）
            statusEl.textContent = '✓';
            statusEl.className = 'status success';
        });

        // 更新总进度（全部提交成功）
        elements.progressBar.style.width = '100%';
        elements.progressText.textContent = `${result.total}/${result.total}`;

        // 显示结果提示(按业务类型区分文案)
        if (state.uploadType === '仓库') {
            // 仓库:仅保存在应用中,不上传到用友云
            showToast(`已提交${result.total}张图片，正在后台保存到应用中...`, 'success');
        } else {
            // 物流:保留原用友云上传文案
            showToast(`已提交${result.total}张图片，正在后台上传到用友云...`, 'success');
        }

        // 3秒后清空并提示查看历史
        setTimeout(() => {
            clearFiles();
            elements.progressSection.style.display = 'none';
            showToast('可在"查看上传历史"中查看最终上传状态', 'info');
        }, 3000);

    } catch (error) {
        showToast(error.message, 'error');
    } finally {
        state.uploading = false;
        elements.btnUpload.disabled = false;
    }
}

// 显示历史记录
async function showHistory() {
    // 必须先选择业务类型,避免混淆物流/仓库历史
    if (!state.uploadType) {
        showToast('请先选择业务类型(物流/仓库)', 'error');
        return;
    }

    try {
        // 始终携带所选业务类型,确保物流/仓库历史互不混淆
        const historyUrl = `/api/history/${state.businessId}?upload_type=${encodeURIComponent(state.uploadType)}`;
        const response = await fetch(historyUrl);
        const result = await response.json();

        if (!response.ok) {
            throw new Error('获取历史记录失败');
        }

        // 渲染历史记录
        if (result.total_count === 0) {
            elements.historyList.innerHTML = '<p style="text-align: center; color: #999;">暂无上传记录</p>';
        } else {
            elements.historyList.innerHTML = result.records.map(record => {
                // 状态文字映射
                const statusText = {
                    'pending': '等待中',
                    'uploading': '上传中',
                    'success': '成功',
                    'failed': '失败'
                };

                return `
                    <div class="history-item">
                        <div class="filename">
                            ${record.file_name}
                            <span class="status-badge ${record.status}">
                                ${statusText[record.status] || record.status}
                            </span>
                        </div>
                        <div class="meta">
                            <div>大小: ${formatFileSize(record.file_size)}</div>
                            <div>时间: ${formatDateTime(record.upload_time)}</div>
                            ${record.error_message ? `<div style="color: #ff4d4f;">错误: ${record.error_message}</div>` : ''}
                        </div>
                    </div>
                `;
            }).join('');
        }

        elements.historyModal.style.display = 'flex';

    } catch (error) {
        showToast(error.message, 'error');
    }
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

// 格式化文件大小
function formatFileSize(bytes) {
    if (bytes < 1024) return bytes + ' B';
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
    return (bytes / 1024 / 1024).toFixed(1) + ' MB';
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

    // 降级处理：尝试移除毫秒和时区偏移后直接返回
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

// 启动应用
init();
