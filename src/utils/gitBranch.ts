export function validBranch(value: string) {
    return value.length > 0 && value.length <= 256 && !value.startsWith('-') && !value.startsWith('/')
        && !value.endsWith('/') && !value.endsWith('.') && !/[\s\x00-\x1f\x7f~^:?*\[\\]/.test(value)
        && !value.includes('..') && !value.includes('@{') && value !== '@'
        && value.split('/').every((part) => part && !part.startsWith('.') && !part.endsWith('.lock'));
}
