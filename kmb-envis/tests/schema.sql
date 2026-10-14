CREATE TABLE dbo.DatabaseUpdates (
    [key] int IDENTITY(1,1) NOT NULL CONSTRAINT PK_DatabaseUpdates PRIMARY KEY,
    Major int NULL, Minor int NULL, Build int NULL, Revision int NULL,
    TimeOfUpdate datetime NULL
);
CREATE TABLE dbo.ENVDBUpdates (
    [Time] datetime2 NULL,
    Major int NULL, Minor int NULL, Build int NULL, Revision int NULL,
    Software int NULL
);
CREATE TABLE dbo.SmpObjectDB (
    Parent int NULL,
    UserName nvarchar(128) NULL,
    Id int IDENTITY(1,1) NOT NULL CONSTRAINT PK_SmpObjectDB PRIMARY KEY,
    objekt nvarchar(128) NULL,
    CONSTRAINT FK_SmpObjectDB_Parent FOREIGN KEY (Parent) REFERENCES dbo.SmpObjectDB (Id)
);
CREATE INDEX iParent_SmpObjectDB ON dbo.SmpObjectDB (Parent);
CREATE UNIQUE INDEX iParentobjekt_SmpObjectDB ON dbo.SmpObjectDB (Parent, objekt);
CREATE TABLE dbo.ArchiveDefinition (
    Name nvarchar(40) NOT NULL CONSTRAINT PK_ArchiveDefinition PRIMARY KEY,
    TypeXML xml NULL
);
CREATE TABLE dbo.SmpIdentifyDB (
    AdeviceAddress nvarchar(32) NULL,
    ArchiveDef nvarchar(40) NULL,
    DeviceNo numeric(10,0) NULL,
    SoftwareVersion numeric(5,0) NULL,
    HardwareVersion numeric(5,0) NULL,
    SoftwareModules numeric(5,0) NULL,
    DeviceAddr tinyint NULL,
    BootloaderVersion tinyint NULL,
    Archives numeric(20,0) NULL,
    Id int IDENTITY(1,1) NOT NULL CONSTRAINT PK_SmpIdentifyDB PRIMARY KEY,
    objekt int NULL,
    DeviceTypeDB numeric(5,0) NULL,
    SubDeviceTypeDB numeric(5,0) NULL,
    SubDeviceType2DB numeric(5,0) NULL,
    SubDeviceType3DB numeric(5,0) NULL,
    SubDeviceType4DB numeric(5,0) NULL,
    SubDeviceType5DB numeric(5,0) NULL,
    SubDeviceType6DB numeric(5,0) NULL,
    PropsTypeDB numeric(5,0) NULL,
    UserName nvarchar(100) NULL,
    CONSTRAINT FK_SmpIdentifyDB_ArchiveDef FOREIGN KEY (ArchiveDef) REFERENCES dbo.ArchiveDefinition (Name),
    CONSTRAINT FK_SmpIdentifyDB_objekt FOREIGN KEY (objekt) REFERENCES dbo.SmpObjectDB (Id)
);
CREATE INDEX iArchiveDef_SmpIdentifyDB ON dbo.SmpIdentifyDB (ArchiveDef);
CREATE INDEX iobjekt_SmpIdentifyDB ON dbo.SmpIdentifyDB (objekt);
CREATE UNIQUE INDEX iobjektDeviceNoPropsTypeDBDeviceTypeDBAdeviceAddressArchiveDefUserName_SmpIdentifyDB
    ON dbo.SmpIdentifyDB (objekt, DeviceNo, PropsTypeDB, DeviceTypeDB, AdeviceAddress, ArchiveDef, UserName);
CREATE TABLE dbo.SmpMeasNameDB (
    LastTime datetime NULL,
    UserName nvarchar(128) NULL,
    Id int IDENTITY(1,1) NOT NULL CONSTRAINT PK_SmpMeasNameDB PRIMARY KEY,
    identifyDB int NULL,
    measName nvarchar(128) NULL,
    CONSTRAINT FK_SmpMeasNameDB_identifyDB FOREIGN KEY (identifyDB) REFERENCES dbo.SmpIdentifyDB (Id)
);
CREATE INDEX iidentifyDB_SmpMeasNameDB ON dbo.SmpMeasNameDB (identifyDB);
CREATE UNIQUE INDEX iidentifyDBmeasName_SmpMeasNameDB ON dbo.SmpMeasNameDB (identifyDB, measName);
CREATE TABLE dbo.SmpDeviceUrl (
    [Time] datetime NULL,
    keydeviceUrl nvarchar(256) NOT NULL,
    keyIdentifyId int NOT NULL,
    CONSTRAINT PK_SmpDeviceUrl PRIMARY KEY (keydeviceUrl, keyIdentifyId),
    CONSTRAINT FK_SmpDeviceUrl_keyIdentifyId FOREIGN KEY (keyIdentifyId) REFERENCES dbo.SmpIdentifyDB (Id)
);
CREATE INDEX ikeyIdentifyId_SmpDeviceUrl ON dbo.SmpDeviceUrl (keyIdentifyId);
CREATE TABLE dbo.UniArchiveDefinition (
    Id int IDENTITY(1,1) NOT NULL CONSTRAINT PK_UniArchiveDefinition PRIMARY KEY,
    CRC numeric(10,0) NULL,
    TypeXML xml NULL,
    Len int NULL
);
CREATE INDEX iCRC_UniArchiveDefinition ON dbo.UniArchiveDefinition (CRC);
CREATE TABLE dbo.UniArchiveBinPack (
    keymeasName int NOT NULL,
    keyArchID tinyint NOT NULL,
    keyTime datetime2(7) NOT NULL,
    ArchDef int NULL,
    Count numeric(10,0) NULL,
    endTime datetime2(7) NULL,
    Period numeric(10,0) NULL,
    TimeIndexes varbinary(max) NULL,
    StreamId uniqueidentifier ROWGUIDCOL NOT NULL CONSTRAINT UQ_UniArchiveBinPack_StreamId UNIQUE,
    DataBuf varbinary(max) NULL,
    CONSTRAINT PK_UniArchiveBinPack PRIMARY KEY (keymeasName, keyArchID, keyTime),
    CONSTRAINT FK_UniArchiveBinPack_ArchDef FOREIGN KEY (ArchDef) REFERENCES dbo.UniArchiveDefinition (Id),
    CONSTRAINT FK_UniArchiveBinPack_keymeasName FOREIGN KEY (keymeasName) REFERENCES dbo.SmpMeasNameDB (Id) ON DELETE CASCADE
);
CREATE INDEX iArchDef_UniArchiveBinPack ON dbo.UniArchiveBinPack (ArchDef);
CREATE INDEX ikeymeasName_UniArchiveBinPack ON dbo.UniArchiveBinPack (keymeasName);
CREATE TABLE dbo.UniConfigBin (
    Id int IDENTITY(1,1) NOT NULL CONSTRAINT PK_UniConfigBin PRIMARY KEY,
    CRC numeric(10,0) NULL,
    Data varbinary(max) NULL
);
CREATE INDEX iCRC_UniConfigBin ON dbo.UniConfigBin (CRC);
CREATE TABLE dbo.UniConfig (
    keymeasName int NOT NULL,
    keyTime datetime NOT NULL,
    Description int NULL,
    Data int NULL,
    CONSTRAINT PK_UniConfig PRIMARY KEY (keymeasName, keyTime),
    CONSTRAINT FK_UniConfig_Data FOREIGN KEY (Data) REFERENCES dbo.UniConfigBin (Id),
    CONSTRAINT FK_UniConfig_Description FOREIGN KEY (Description) REFERENCES dbo.UniConfigBin (Id),
    CONSTRAINT FK_UniConfig_keymeasName FOREIGN KEY (keymeasName) REFERENCES dbo.SmpMeasNameDB (Id) ON DELETE CASCADE
);
CREATE TABLE dbo.DState (
    keyArchID tinyint NOT NULL,
    keymeasName int NOT NULL,
    keyTime datetime NOT NULL,
    endTime datetime NULL,
    [From] int NULL,
    [To] int NULL,
    reason tinyint NULL,
    CONSTRAINT PK_DState PRIMARY KEY (keyArchID, keymeasName, keyTime),
    CONSTRAINT FK_DState_keymeasName FOREIGN KEY (keymeasName) REFERENCES dbo.SmpMeasNameDB (Id) ON DELETE CASCADE
);
CREATE INDEX ikeymeasName_DState ON dbo.DState (keymeasName);
INSERT INTO dbo.DatabaseUpdates (Major, Minor, Build, Revision, TimeOfUpdate)
    VALUES (2, 5, 25, 17755, '2026-09-01T06:24:09');
INSERT INTO dbo.ENVDBUpdates ([Time], Major, Minor, Build, Revision, Software)
    VALUES ('2025-01-15T07:50:15', 1, 2, 0, 14671, 1);
