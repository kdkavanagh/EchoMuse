package client

// DeviceStats is the retained stats message body. Unavailable measurements
// use nil rather than a numeric sentinel (SPEC §8.1).
type DeviceStats struct {
	CPUPct           float64  `json:"cpuPct"`
	MemUsedMb        int      `json:"memUsedMb"`
	MemTotalMb       int      `json:"memTotalMb"`
	StorageUsedMb    int      `json:"storageUsedMb"`
	StorageTotalMb   int      `json:"storageTotalMb"`
	WifiRssi         *int     `json:"wifiRssi"`
	WifiSsid         string   `json:"wifiSsid"`
	LinkSpeedMbps    int      `json:"linkSpeedMbps,omitempty"`
	WifiFreqMhz      int      `json:"wifiFreqMhz,omitempty"`
	WifiBssid        string   `json:"wifiBssid,omitempty"`
	TxBytes          uint64   `json:"txBytes"`
	RxBytes          uint64   `json:"rxBytes"`
	TxErrors         uint64   `json:"txErrors"`
	TxDropped        uint64   `json:"txDropped"`
	RxCrcErrors      uint64   `json:"rxCrcErrors"`
	Ble              any      `json:"ble,omitempty"`
	AmbientLux       *int     `json:"ambientLux"`
	CPUTempC         *float64 `json:"cpuTempC"`
	MaxTempC         *float64 `json:"maxTempC"`
	CoresOnline      int      `json:"coresOnline,omitempty"`
	CoresTotal       int      `json:"coresTotal,omitempty"`
	ThermalCoreLimit int      `json:"thermalCoreLimit,omitempty"`
}
