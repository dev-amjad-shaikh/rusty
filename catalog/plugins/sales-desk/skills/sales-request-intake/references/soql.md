# SOQL the desk uses (Salesforce)
- Opportunity: `SELECT Id, Name, StageName, Amount, CloseDate, Probability, ForecastCategoryName, NextStep, LastActivityDate, LastModifiedDate, Owner.Name, Account.Name, Type FROM Opportunity WHERE Name LIKE '%<name>%' ORDER BY LastModifiedDate DESC LIMIT 5`
- Products on it: `SELECT PricebookEntry.Product2.Name, Quantity, UnitPrice, TotalPrice, Discount FROM OpportunityLineItem WHERE OpportunityId='006...'`
- Account and its open deals: `SELECT Id, Name, Industry, Owner.Name, AnnualRevenue, (SELECT Name, StageName, Amount, CloseDate FROM Opportunities WHERE IsClosed=false) FROM Account WHERE Name LIKE '%<name>%'`
- Last activity: `SELECT Subject, ActivityDate, Owner.Name FROM Task WHERE WhatId='006...' ORDER BY ActivityDate DESC LIMIT 5`
- Contacts: `SELECT Name, Title, Email, Phone FROM Contact WHERE AccountId='001...'`
- Open cases (support health): `SELECT CaseNumber, Subject, Status, Priority, CreatedDate FROM Case WHERE AccountId='001...' AND IsClosed=false`
- Quotes (CPQ): `SELECT Name, SBQQ__Status__c, SBQQ__NetAmount__c, SBQQ__ExpirationDate__c, SBQQ__Primary__c FROM SBQQ__Quote__c WHERE SBQQ__Opportunity2__c='006...'`
- Contracts: `SELECT ContractNumber, Status, StartDate, EndDate, ContractTerm, Account.Name FROM Contract WHERE AccountId='001...'`
- Orders: `SELECT OrderNumber, Status, EffectiveDate, TotalAmount FROM Order WHERE AccountId='001...' ORDER BY EffectiveDate DESC`
- Stale pipeline for a rep: `SELECT Name, StageName, Amount, CloseDate, LastModifiedDate FROM Opportunity WHERE Owner.Name='<rep>' AND IsClosed=false AND (CloseDate < TODAY OR LastModifiedDate < LAST_N_DAYS:30)`
Field names vary by org: `salesforce.list-objects` and the org's data dictionary in knowledge win over this page.
