// SPDX-License-Identifier: MIT

use vehicle_information_service_client::*;

#[tokio::test]
async fn receive_get_async() -> Result<(), VISClientError> {
    let client = VISClient::connect("ws://127.0.0.1:14430").await?;
    let timestamp: u32 = client.get("Private.Example.Timestamp".into()).await?;
    assert!(timestamp > 0);

    Ok(())
}

#[tokio::test]
async fn get_invalid_path_should_return_invalid_path() -> Result<(), VISClientError> {
    let client = VISClient::connect("ws://127.0.0.1:14430").await?;
    let response: Result<u32, VISClientError> = client.get("Invalid.Path".into()).await;

    if let Err(VISClientError::VisError(ActionErrorResponse::Get {
        request_id,
        error,
        timestamp: _,
    })) = response
    {
        if let ReqID::ReqIDUUID(req_uuid) = request_id {
            assert!(!req_uuid.is_nil());
        } else {
            panic!("Unexpected request id type");
        }
        assert_eq!(404, error.number);
        assert_eq!("invalid_path".to_string(), error.reason);
        assert_eq!(
            "The specified data path does not exist.".to_string(),
            error.message
        );
    } else {
        panic!("Unexpected success for invalid path: {:#?}", response);
    }

    Ok(())
}
